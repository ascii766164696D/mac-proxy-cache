use std::path::{Path, PathBuf};

/// Map a Content-Type to a file extension.
pub fn content_type_to_ext(content_type: &str) -> &'static str {
    // Take the mime type part before any parameters (e.g., "text/html; charset=utf-8" -> "text/html")
    let mime = content_type.split(';').next().unwrap_or("").trim().to_lowercase();

    match mime.as_str() {
        "text/html" => ".html",
        "text/css" => ".css",
        "text/plain" => ".txt",
        "text/xml" | "application/xml" => ".xml",
        "application/javascript" | "text/javascript" => ".js",
        "application/json" => ".json",
        "image/png" => ".png",
        "image/jpeg" => ".jpg",
        "image/gif" => ".gif",
        "image/svg+xml" => ".svg",
        "image/webp" => ".webp",
        "image/avif" => ".avif",
        "image/x-icon" | "image/vnd.microsoft.icon" => ".ico",
        "video/mp4" => ".mp4",
        "video/webm" => ".webm",
        "audio/mpeg" => ".mp3",
        "audio/mp4" => ".m4a",
        "audio/webm" => ".weba",
        "audio/ogg" => ".ogg",
        "application/x-mpegurl" | "application/vnd.apple.mpegurl" => ".m3u8",
        "video/mp2t" => ".ts",
        "application/dash+xml" => ".mpd",
        "application/wasm" => ".wasm",
        "font/woff" => ".woff",
        "font/woff2" => ".woff2",
        "font/ttf" | "application/x-font-ttf" => ".ttf",
        "font/otf" => ".otf",
        "application/pdf" => ".pdf",
        "application/zip" => ".zip",
        "application/gzip" => ".gz",
        _ => ".bin",
    }
}

/// Classify a Content-Type into a media type category.
pub fn classify_media_type(content_type: &str) -> Option<&'static str> {
    let mime = content_type.split(';').next().unwrap_or("").trim().to_lowercase();
    if mime.starts_with("image/") {
        Some("image")
    } else if mime.starts_with("video/") || mime == "application/x-mpegurl" || mime == "application/vnd.apple.mpegurl" || mime == "application/dash+xml" {
        Some("video")
    } else if mime.starts_with("audio/") {
        Some("audio")
    } else {
        None
    }
}

/// Known file extensions that we recognize from URLs.
const KNOWN_EXTENSIONS: &[&str] = &[
    ".html", ".htm", ".css", ".js", ".mjs", ".json", ".xml",
    ".png", ".jpg", ".jpeg", ".gif", ".svg", ".webp", ".avif", ".ico", ".bmp",
    ".mp4", ".webm", ".m4v", ".mov", ".avi",
    ".mp3", ".m4a", ".weba", ".ogg", ".wav", ".flac",
    ".m3u8", ".ts", ".m4s", ".mpd",
    ".wasm", ".woff", ".woff2", ".ttf", ".otf", ".eot",
    ".pdf", ".zip", ".gz", ".tar",
    ".txt", ".csv", ".md",
];

/// Check if a URL path already has a recognized file extension.
fn has_known_extension(path: &str) -> bool {
    let lower = path.to_lowercase();
    KNOWN_EXTENSIONS.iter().any(|ext| lower.ends_with(ext))
}

/// Sanitize a single path segment: replace unsafe filesystem chars.
fn sanitize_segment(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            ':' | '*' | '?' | '"' | '<' | '>' | '|' | '\\' | '\0' => '_',
            _ => c,
        })
        .collect()
}

/// Build a browsable filesystem path from a URL and response metadata.
///
/// Returns a relative path like `example.com/static/logo.png`.
pub fn url_to_cache_path(
    method: &str,
    url: &str,
    content_type: Option<&str>,
    vary_key: Option<&str>,
) -> PathBuf {
    let parsed = match url::Url::parse(url) {
        Ok(u) => u,
        Err(_) => return PathBuf::from("_invalid_url_.bin"),
    };

    let host = parsed.host_str().unwrap_or("_unknown_host_");
    let path = parsed.path();
    let query = parsed.query();

    let mut result = PathBuf::new();

    // Domain directory
    result.push(sanitize_segment(host));

    // Method prefix for non-GET
    if method != "GET" {
        result.push(format!("_{}_", method));
    }

    // Path segments
    let path = path.strip_prefix('/').unwrap_or(path);

    if path.is_empty() || path == "/" {
        // Root index
        let ext = content_type.map(content_type_to_ext).unwrap_or(".html");
        let mut filename = format!("_index_{}", ext);
        if let Some(vk) = vary_key {
            filename = insert_vary_suffix(&filename, vk);
        }
        result.push(filename);
    } else {
        // Split into directory parts + final filename
        let segments: Vec<&str> = path.split('/').collect();
        let (dirs, file_part) = segments.split_at(segments.len() - 1);

        for dir in dirs {
            if !dir.is_empty() {
                result.push(sanitize_segment(dir));
            }
        }

        let file = file_part[0];
        let file = if file.is_empty() {
            // Trailing slash, e.g., /api/users/
            "_index_"
        } else {
            file
        };

        let sanitized = sanitize_segment(file);

        // Build the filename with query and vary suffixes
        let mut filename = if let Some(q) = query {
            add_query_suffix(&sanitized, q, content_type)
        } else if !has_known_extension(&sanitized) {
            // No extension in URL — add one from Content-Type
            let ext = content_type.map(content_type_to_ext).unwrap_or(".bin");
            format!("{}{}", sanitized, ext)
        } else {
            sanitized
        };

        if let Some(vk) = vary_key {
            filename = insert_vary_suffix(&filename, vk);
        }

        result.push(filename);
    }

    // Safety: ensure total path length is reasonable
    let path_str = result.to_string_lossy();
    if path_str.len() > 200 {
        let ext = content_type.map(content_type_to_ext).unwrap_or(".bin");
        let hash = &crate::cache::key::compute_fingerprint(method, url, &[])[..12];
        return PathBuf::from(sanitize_segment(host))
            .join(format!("_long_{}{}", hash, ext));
    }

    result
}

/// Add query string suffix to filename: `users~q~page=1.json`
fn add_query_suffix(file: &str, query: &str, content_type: Option<&str>) -> String {
    // Determine extension
    let (stem, ext) = if has_known_extension(file) {
        let dot_pos = file.rfind('.').unwrap();
        (&file[..dot_pos], &file[dot_pos..])
    } else {
        let ext = content_type.map(content_type_to_ext).unwrap_or(".bin");
        (file, ext)
    };

    // Sanitize and possibly truncate query
    let sanitized_query = sanitize_segment(query);
    if sanitized_query.len() > 60 {
        let hash = &hex::encode(sha2::Sha256::digest(query.as_bytes()))[..8];
        format!("{}~q~{}_{}{}", stem, &sanitized_query[..40], hash, ext)
    } else {
        format!("{}~q~{}{}", stem, sanitized_query, ext)
    }
}

/// Insert a Vary suffix before the extension: `app~v~gzip.js`
fn insert_vary_suffix(filename: &str, vary_key: &str) -> String {
    if let Some(dot_pos) = filename.rfind('.') {
        let stem = &filename[..dot_pos];
        let ext = &filename[dot_pos..];
        format!("{}~v~{}{}", stem, vary_key, ext)
    } else {
        format!("{}~v~{}", filename, vary_key)
    }
}

/// Write body bytes to the cache, using atomic rename.
pub fn write_body(cache_dir: &Path, relative_path: &Path, body: &[u8]) -> Result<(), std::io::Error> {
    let full_path = cache_dir.join(relative_path);
    if let Some(parent) = full_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    // Write to temp file in same directory, then atomic rename
    let temp_path = full_path.with_extension("tmp");
    std::fs::write(&temp_path, body)?;
    std::fs::rename(&temp_path, &full_path)?;

    Ok(())
}

/// Read body bytes from the cache.
pub fn read_body(cache_dir: &Path, relative_path: &Path) -> Result<Vec<u8>, std::io::Error> {
    let full_path = cache_dir.join(relative_path);
    std::fs::read(&full_path)
}

/// Rename a cached file to a stale variant with timestamp.
pub fn rename_to_stale(cache_dir: &Path, relative_path: &Path) -> Result<PathBuf, std::io::Error> {
    let full_path = cache_dir.join(relative_path);
    if !full_path.exists() {
        return Ok(relative_path.to_path_buf());
    }

    let timestamp = chrono_timestamp();
    let stale_path = make_stale_path(relative_path, &timestamp);
    let full_stale = cache_dir.join(&stale_path);

    std::fs::rename(&full_path, &full_stale)?;
    Ok(stale_path)
}

fn chrono_timestamp() -> String {
    use std::time::SystemTime;
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = now.as_secs();
    // Simple timestamp format: YYYY-MM-DDTHHhMM
    let days = secs / 86400;
    let time_of_day = secs % 86400;
    let hours = time_of_day / 3600;
    let minutes = (time_of_day % 3600) / 60;
    // Approximate date calculation (good enough for filenames)
    let (year, month, day) = days_to_date(days);
    format!("{:04}-{:02}-{:02}T{:02}h{:02}", year, month, day, hours, minutes)
}

fn days_to_date(days_since_epoch: u64) -> (u64, u64, u64) {
    // Simplified date calculation from Unix days
    let mut y = 1970;
    let mut remaining = days_since_epoch;
    loop {
        let days_in_year = if is_leap(y) { 366 } else { 365 };
        if remaining < days_in_year {
            break;
        }
        remaining -= days_in_year;
        y += 1;
    }
    let days_in_months: [u64; 12] = if is_leap(y) {
        [31, 29, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    } else {
        [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    };
    let mut m = 1;
    for dim in &days_in_months {
        if remaining < *dim {
            break;
        }
        remaining -= dim;
        m += 1;
    }
    (y, m, remaining + 1)
}

fn is_leap(y: u64) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

/// Create a stale path from an original path: `logo.png` -> `logo~stale~2026-03-14T15h30.png`
fn make_stale_path(path: &Path, timestamp: &str) -> PathBuf {
    let filename = path.file_name().unwrap_or_default().to_string_lossy();
    let stale_filename = if let Some(dot_pos) = filename.rfind('.') {
        let stem = &filename[..dot_pos];
        let ext = &filename[dot_pos..];
        format!("{}~stale~{}{}", stem, timestamp, ext)
    } else {
        format!("{}~stale~{}", filename, timestamp)
    };

    path.with_file_name(stale_filename)
}

/// Delete a cached file permanently.
pub fn delete_file(cache_dir: &Path, relative_path: &Path) -> Result<(), std::io::Error> {
    let full_path = cache_dir.join(relative_path);
    if full_path.exists() {
        std::fs::remove_file(&full_path)?;
    }
    Ok(())
}

use sha2::Digest;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_simple_path() {
        let path = url_to_cache_path("GET", "https://example.com/static/logo.png", Some("image/png"), None);
        assert_eq!(path, PathBuf::from("example.com/static/logo.png"));
    }

    #[test]
    fn test_root_index() {
        let path = url_to_cache_path("GET", "https://example.com/", Some("text/html"), None);
        assert_eq!(path, PathBuf::from("example.com/_index_.html"));
    }

    #[test]
    fn test_no_extension_infer_from_content_type() {
        let path = url_to_cache_path("GET", "https://example.com/api/users", Some("application/json"), None);
        assert_eq!(path, PathBuf::from("example.com/api/users.json"));
    }

    #[test]
    fn test_query_string() {
        let path = url_to_cache_path("GET", "https://example.com/api/users?page=1", Some("application/json"), None);
        assert_eq!(path, PathBuf::from("example.com/api/users~q~page=1.json"));
    }

    #[test]
    fn test_vary_suffix() {
        let path = url_to_cache_path("GET", "https://example.com/app.js", Some("application/javascript"), Some("gzip"));
        assert_eq!(path, PathBuf::from("example.com/app~v~gzip.js"));
    }

    #[test]
    fn test_non_get_method() {
        let path = url_to_cache_path("HEAD", "https://example.com/page", Some("text/html"), None);
        assert_eq!(path, PathBuf::from("example.com/_HEAD_/page.html"));
    }

    #[test]
    fn test_unsafe_chars_sanitized() {
        let path = url_to_cache_path("GET", "https://example.com/file:name", Some("text/plain"), None);
        let filename = path.file_name().unwrap().to_string_lossy();
        assert!(!filename.contains(':'));
    }

    #[test]
    fn test_content_type_to_ext() {
        assert_eq!(content_type_to_ext("text/html; charset=utf-8"), ".html");
        assert_eq!(content_type_to_ext("application/json"), ".json");
        assert_eq!(content_type_to_ext("image/webp"), ".webp");
        assert_eq!(content_type_to_ext("video/mp4"), ".mp4");
        assert_eq!(content_type_to_ext("unknown/type"), ".bin");
    }

    #[test]
    fn test_classify_media_type() {
        assert_eq!(classify_media_type("image/png"), Some("image"));
        assert_eq!(classify_media_type("video/mp4"), Some("video"));
        assert_eq!(classify_media_type("audio/mpeg"), Some("audio"));
        assert_eq!(classify_media_type("text/html"), None);
        assert_eq!(classify_media_type("application/x-mpegurl"), Some("video"));
    }

    #[test]
    fn test_make_stale_path() {
        let path = Path::new("example.com/logo.png");
        let stale = make_stale_path(path, "2026-03-14T15h30");
        assert_eq!(stale, PathBuf::from("example.com/logo~stale~2026-03-14T15h30.png"));
    }

    #[test]
    fn test_write_and_read_body() {
        let tmp = std::env::temp_dir().join("mac-proxy-cache-test");
        let _ = std::fs::remove_dir_all(&tmp);
        let rel = PathBuf::from("test.com/file.txt");

        write_body(&tmp, &rel, b"hello world").unwrap();
        let body = read_body(&tmp, &rel).unwrap();
        assert_eq!(body, b"hello world");

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_rename_to_stale() {
        let tmp = std::env::temp_dir().join("mac-proxy-cache-test-stale");
        let _ = std::fs::remove_dir_all(&tmp);
        let rel = PathBuf::from("test.com/file.txt");

        write_body(&tmp, &rel, b"content").unwrap();
        let stale_path = rename_to_stale(&tmp, &rel).unwrap();

        // Original should not exist
        assert!(!tmp.join(&rel).exists());
        // Stale should exist
        assert!(tmp.join(&stale_path).exists());
        assert!(stale_path.to_string_lossy().contains("~stale~"));

        let _ = std::fs::remove_dir_all(&tmp);
    }
}
