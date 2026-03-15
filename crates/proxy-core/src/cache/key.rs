use sha2::{Digest, Sha256};

/// Known static resource extensions where query params are cosmetic
/// (quality, format, size hints) and the underlying resource is the same.
const STATIC_EXTENSIONS: &[&str] = &[
    ".jpg", ".jpeg", ".png", ".gif", ".webp", ".avif", ".ico", ".svg",
    ".mp4", ".webm", ".m4a", ".mp3", ".ogg", ".weba",
    ".woff", ".woff2", ".ttf", ".otf", ".eot",
    ".css", ".js",
];

/// Query params that are cosmetic/rendering hints and don't change the resource.
const COSMETIC_PARAMS: &[&str] = &[
    "quality", "q",
    "auto",
    "w", "h", "width", "height",
    "dpr",
    "fit", "crop", "gravity",
    "format", "f", "fm",
    "blur", "sharp",
    "dl", // download flag
    "v", "ver", "version", // cache busters — strip so we cache across versions
];

/// Normalize a URL for cache key purposes.
/// For static resources (images, fonts, JS, CSS), strip cosmetic query params
/// so the same underlying resource maps to one cache entry.
pub fn normalize_url(url: &str) -> String {
    // Parse the URL to inspect the path
    let parsed = match url::Url::parse(url) {
        Ok(u) => u,
        Err(_) => return url.to_string(),
    };

    let path = parsed.path().to_lowercase();

    // Only normalize URLs with known static extensions
    let is_static = STATIC_EXTENSIONS.iter().any(|ext| {
        // Check if path ends with extension (possibly followed by nothing)
        path.ends_with(ext) || path.contains(&format!("{ext}/"))
    });

    if !is_static {
        return url.to_string();
    }

    // Filter out cosmetic query params
    let remaining_params: Vec<(String, String)> = parsed
        .query_pairs()
        .filter(|(k, _)| !COSMETIC_PARAMS.contains(&k.to_lowercase().as_str()))
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();

    // Rebuild URL without cosmetic params
    let mut normalized = parsed.clone();
    if remaining_params.is_empty() {
        normalized.set_query(None);
    } else {
        let qs: Vec<String> = remaining_params
            .iter()
            .map(|(k, v)| {
                if v.is_empty() {
                    k.clone()
                } else {
                    format!("{k}={v}")
                }
            })
            .collect();
        normalized.set_query(Some(&qs.join("&")));
    }

    normalized.to_string()
}

/// Compute a cache fingerprint from method, URL, and Vary header values.
/// The fingerprint is a hex-encoded SHA-256 hash.
pub fn compute_fingerprint(method: &str, url: &str, vary_values: &[(&str, &str)]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(method.as_bytes());
    hasher.update(b"\0");
    hasher.update(url.as_bytes());

    // Sort vary values for deterministic hashing
    let mut sorted: Vec<_> = vary_values.to_vec();
    sorted.sort_by_key(|(k, _)| k.to_lowercase());

    for (name, value) in &sorted {
        hasher.update(b"\0");
        hasher.update(name.to_lowercase().as_bytes());
        hasher.update(b"=");
        hasher.update(value.as_bytes());
    }

    hex::encode(hasher.finalize())
}

/// Extract the Vary header value pairs from response Vary header and request headers.
/// Returns the header names and their corresponding request values.
pub fn extract_vary_values(
    vary_header: Option<&str>,
    request_headers: &[(String, String)],
) -> Vec<(String, String)> {
    let vary = match vary_header {
        Some(v) if v != "*" => v,
        _ => return Vec::new(),
    };

    vary.split(',')
        .map(|name| {
            let name = name.trim().to_lowercase();
            let value = request_headers
                .iter()
                .find(|(k, _)| k.to_lowercase() == name)
                .map(|(_, v)| v.clone())
                .unwrap_or_default();
            (name, value)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_fingerprint_deterministic() {
        let fp1 = compute_fingerprint("GET", "https://example.com/page", &[]);
        let fp2 = compute_fingerprint("GET", "https://example.com/page", &[]);
        assert_eq!(fp1, fp2);
    }

    #[test]
    fn test_fingerprint_different_methods() {
        let fp_get = compute_fingerprint("GET", "https://example.com/page", &[]);
        let fp_head = compute_fingerprint("HEAD", "https://example.com/page", &[]);
        assert_ne!(fp_get, fp_head);
    }

    #[test]
    fn test_fingerprint_with_vary() {
        let fp1 = compute_fingerprint(
            "GET",
            "https://example.com/page",
            &[("accept-encoding", "gzip")],
        );
        let fp2 = compute_fingerprint(
            "GET",
            "https://example.com/page",
            &[("accept-encoding", "br")],
        );
        assert_ne!(fp1, fp2);
    }

    #[test]
    fn test_fingerprint_vary_order_independent() {
        let fp1 = compute_fingerprint(
            "GET",
            "https://example.com/",
            &[("accept-encoding", "gzip"), ("accept-language", "en")],
        );
        let fp2 = compute_fingerprint(
            "GET",
            "https://example.com/",
            &[("accept-language", "en"), ("accept-encoding", "gzip")],
        );
        assert_eq!(fp1, fp2);
    }

    #[test]
    fn test_extract_vary_values() {
        let headers = vec![
            ("Accept-Encoding".to_string(), "gzip, br".to_string()),
            ("Accept-Language".to_string(), "en-US".to_string()),
        ];

        let result = extract_vary_values(Some("Accept-Encoding, Accept-Language"), &headers);
        assert_eq!(result.len(), 2);
        assert_eq!(result[0], ("accept-encoding".to_string(), "gzip, br".to_string()));
        assert_eq!(result[1], ("accept-language".to_string(), "en-US".to_string()));
    }

    #[test]
    fn test_extract_vary_star() {
        let result = extract_vary_values(Some("*"), &[]);
        assert!(result.is_empty());
    }

    #[test]
    fn test_extract_vary_none() {
        let result = extract_vary_values(None, &[]);
        assert!(result.is_empty());
    }

    #[test]
    fn test_normalize_strips_cosmetic_params() {
        let url = "https://static01.nyt.com/images/2026/photo.jpg?quality=75&auto=webp";
        let normalized = normalize_url(url);
        assert_eq!(normalized, "https://static01.nyt.com/images/2026/photo.jpg");
    }

    #[test]
    fn test_normalize_keeps_non_cosmetic_params() {
        let url = "https://cdn.example.com/image.png?id=abc123&quality=90";
        let normalized = normalize_url(url);
        assert_eq!(normalized, "https://cdn.example.com/image.png?id=abc123");
    }

    #[test]
    fn test_normalize_ignores_non_static() {
        let url = "https://api.example.com/data?quality=75&page=2";
        let normalized = normalize_url(url);
        assert_eq!(normalized, url);
    }

    #[test]
    fn test_normalize_same_fingerprint() {
        let url1 = "https://cdn.example.com/photo.jpg?quality=75&auto=webp";
        let url2 = "https://cdn.example.com/photo.jpg?quality=90&auto=avif";
        let fp1 = compute_fingerprint("GET", &normalize_url(url1), &[]);
        let fp2 = compute_fingerprint("GET", &normalize_url(url2), &[]);
        assert_eq!(fp1, fp2);
    }
}
