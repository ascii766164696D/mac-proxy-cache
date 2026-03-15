use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio_rusqlite::Connection;

use crate::cache::store;
use crate::error::Error;

/// Parsed Content-Range header: `bytes start-end/total` or `bytes start-end/*`.
#[derive(Debug, Clone)]
pub struct ContentRange {
    pub start: u64,
    pub end: u64,
    pub total: Option<u64>,
}

impl ContentRange {
    /// Parse a Content-Range header value like "bytes 0-1023/4096" or "bytes 0-1023/*".
    pub fn parse(header: &str) -> Option<Self> {
        let s = header.strip_prefix("bytes ")?;
        let (range_part, total_part) = s.split_once('/')?;
        let (start_s, end_s) = range_part.split_once('-')?;

        let start = start_s.trim().parse().ok()?;
        let end = end_s.trim().parse().ok()?;
        let total = if total_part.trim() == "*" {
            None
        } else {
            Some(total_part.trim().parse().ok()?)
        };

        Some(Self { start, end, total })
    }
}

/// Parse a Range request header like "bytes=0-1023".
pub fn parse_range_header(header: &str) -> Option<(u64, Option<u64>)> {
    let s = header.strip_prefix("bytes=")?;
    let (start_s, end_s) = s.split_once('-')?;
    let start = start_s.trim().parse().ok()?;
    let end = if end_s.trim().is_empty() {
        None
    } else {
        Some(end_s.trim().parse().ok()?)
    };
    Some((start, end))
}

/// Manifest stored alongside range slabs in the .parts/ directory.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RangeManifest {
    pub url: String,
    pub total_size: Option<u64>,
    pub content_type: Option<String>,
    pub slabs: Vec<SlabInfo>,
    pub coverage_bytes: u64,
    pub coverage_pct: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SlabInfo {
    pub start: u64,
    pub end: u64,
}

/// Result of a slab lookup — includes the actual byte range that can be served.
#[derive(Debug, Clone)]
pub struct SlabHit {
    pub slab_path: PathBuf,
    /// The slab's full range on disk
    pub slab_start: u64,
    pub slab_end: u64,
    /// The range we can actually serve from this slab (intersection of request and slab)
    pub serve_start: u64,
    pub serve_end: u64,
    pub total: Option<u64>,
}

/// Manager for range-requested resources.
#[derive(Clone)]
pub struct RangeCache {
    conn: Connection,
    cache_dir: PathBuf,
}

impl RangeCache {
    pub fn new(conn: Connection, cache_dir: PathBuf) -> Self {
        Self { conn, cache_dir }
    }

    /// Get or create a range resource entry. Returns the resource ID and dir_path.
    pub async fn get_or_create_resource(
        &self,
        url_pattern: &str,
        host: &str,
        content_type: Option<&str>,
    ) -> Result<(i64, PathBuf), Error> {
        let url = url_pattern.to_string();
        let h = host.to_string();
        let ct = content_type.map(|s| s.to_string());

        self.conn
            .call(move |conn| {
                let existing: Option<(i64, String)> = conn
                    .query_row(
                        "SELECT id, dir_path FROM range_resources WHERE url_pattern = ?1",
                        params![url],
                        |row| Ok((row.get(0)?, row.get(1)?)),
                    )
                    .optional()?;

                if let Some((id, dir_path)) = existing {
                    return Ok((id, PathBuf::from(dir_path)));
                }

                let dir_path = build_parts_dir(&url, ct.as_deref());
                let now = now_unix();

                conn.execute(
                    "INSERT INTO range_resources (url_pattern, host, content_type, dir_path, created_at, last_accessed)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    params![url, h, ct, dir_path.to_string_lossy().to_string(), now, now],
                )?;

                let id = conn.last_insert_rowid();
                Ok((id, dir_path))
            })
            .await
            .map_err(|e| Error::Proxy(format!("Failed to get/create range resource: {}", e)))
    }

    /// Store a range slab (from a 206 response).
    pub async fn store_slab(
        &self,
        resource_id: i64,
        dir_path: &Path,
        range: &ContentRange,
        body: &[u8],
    ) -> Result<(), Error> {
        let slab_filename = format!(
            "{:012}-{:012}.part",
            range.start, range.end
        );
        let slab_rel_path = dir_path.join(&slab_filename);

        // Write slab to disk
        store::write_body(&self.cache_dir, &slab_rel_path, body)?;

        // Insert into SQLite
        let slab_path = slab_rel_path.to_string_lossy().to_string();
        let start = range.start as i64;
        let end = range.end as i64;
        let total = range.total.map(|t| t as i64);
        let rid = resource_id;

        self.conn
            .call(move |conn| {
                conn.execute(
                    "INSERT OR REPLACE INTO range_slabs (resource_id, range_start, range_end, slab_path)
                     VALUES (?1, ?2, ?3, ?4)",
                    params![rid, start, end, slab_path],
                )?;

                if let Some(total) = total {
                    conn.execute(
                        "UPDATE range_resources SET total_size = ?1 WHERE id = ?2 AND total_size IS NULL",
                        params![total, rid],
                    )?;
                }

                conn.execute(
                    "UPDATE range_resources SET last_accessed = ?1 WHERE id = ?2",
                    params![now_unix(), rid],
                )?;

                Ok(())
            })
            .await
            .map_err(|e| Error::Proxy(format!("Failed to store range slab: {}", e)))?;

        self.write_manifest(resource_id, dir_path).await?;

        Ok(())
    }

    /// Find a slab that fully covers the requested byte range.
    /// Returns None if no single slab covers the entire request.
    pub async fn find_covering_slab(
        &self,
        url_pattern: &str,
        req_start: u64,
        req_end: Option<u64>,
    ) -> Result<Option<SlabHit>, Error> {
        let url = url_pattern.to_string();
        let s = req_start as i64;

        self.conn
            .call(move |conn| {
                let resource: Option<(i64, Option<i64>)> = conn
                    .query_row(
                        "SELECT id, total_size FROM range_resources WHERE url_pattern = ?1",
                        params![url],
                        |row| Ok((row.get(0)?, row.get(1)?)),
                    )
                    .optional()?;

                let Some((rid, total)) = resource else {
                    return Ok(None);
                };

                // Find a slab that contains the requested start position
                // AND whose end is >= the requested end (full coverage)
                let slab: Option<(String, i64, i64)> = conn
                    .query_row(
                        "SELECT slab_path, range_start, range_end FROM range_slabs
                         WHERE resource_id = ?1 AND range_start <= ?2 AND range_end >= ?2
                         ORDER BY (range_end - range_start) DESC LIMIT 1",
                        params![rid, s],
                        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                    )
                    .optional()?;

                match slab {
                    Some((path, slab_start, slab_end)) => {
                        let slab_start = slab_start as u64;
                        let slab_end = slab_end as u64;
                        let total = total.map(|t| t as u64);

                        // Calculate what we can actually serve
                        let serve_start = req_start;
                        let serve_end = match req_end {
                            Some(re) => re.min(slab_end),
                            None => slab_end,
                        };

                        // Only serve if the slab fully covers the requested range
                        if let Some(re) = req_end {
                            if slab_end < re {
                                // Slab doesn't cover the full requested range
                                return Ok(None);
                            }
                        }

                        Ok(Some(SlabHit {
                            slab_path: PathBuf::from(path),
                            slab_start,
                            slab_end,
                            serve_start,
                            serve_end,
                            total,
                        }))
                    }
                    None => Ok(None),
                }
            })
            .await
            .map_err(|e| Error::Proxy(format!("Failed to find covering slab: {}", e)))
    }

    /// Check if a resource is fully covered and ready for assembly.
    pub async fn check_assembly_ready(&self, resource_id: i64) -> Result<Option<AssemblyInfo>, Error> {
        let rid = resource_id;

        self.conn
            .call(move |conn| {
                let (total_size, dir_path): (Option<i64>, String) = conn.query_row(
                    "SELECT total_size, dir_path FROM range_resources WHERE id = ?1",
                    params![rid],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )?;

                let Some(total) = total_size else {
                    return Ok(None);
                };

                let mut stmt = conn.prepare(
                    "SELECT range_start, range_end, slab_path FROM range_slabs
                     WHERE resource_id = ?1 ORDER BY range_start ASC",
                )?;
                let slabs: Vec<(i64, i64, String)> = stmt
                    .query_map(params![rid], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
                    .collect::<Result<Vec<_>, _>>()?;

                if slabs.is_empty() {
                    return Ok(None);
                }

                let mut covered_end: i64 = -1;
                for (start, end, _) in &slabs {
                    if *start > covered_end + 1 {
                        return Ok(None);
                    }
                    if *end > covered_end {
                        covered_end = *end;
                    }
                }

                if covered_end + 1 >= total {
                    Ok(Some(AssemblyInfo {
                        dir_path: PathBuf::from(dir_path),
                        total_size: total as u64,
                        slabs: slabs
                            .into_iter()
                            .map(|(s, e, p)| (s as u64, e as u64, PathBuf::from(p)))
                            .collect(),
                    }))
                } else {
                    Ok(None)
                }
            })
            .await
            .map_err(|e| Error::Proxy(format!("Failed to check assembly: {}", e)))
    }

    /// Assemble a complete file from range slabs.
    pub async fn assemble(
        &self,
        resource_id: i64,
        info: AssemblyInfo,
        url: &str,
        content_type: Option<&str>,
        host: &str,
    ) -> Result<PathBuf, Error> {
        let cache_dir = self.cache_dir.clone();

        let dir_str = info.dir_path.to_string_lossy().to_string();
        let final_path = if let Some(stripped) = dir_str.strip_suffix(".parts") {
            PathBuf::from(stripped)
        } else {
            info.dir_path.with_extension("")
        };

        let mut assembled = Vec::with_capacity(info.total_size as usize);
        let mut current_pos: u64 = 0;

        for (start, end, slab_path) in &info.slabs {
            let data = store::read_body(&cache_dir, slab_path)?;
            if *start < current_pos {
                let skip = (current_pos - start) as usize;
                if skip < data.len() {
                    assembled.extend_from_slice(&data[skip..]);
                }
            } else {
                assembled.extend_from_slice(&data);
            }
            current_pos = end + 1;
        }

        store::write_body(&cache_dir, &final_path, &assembled)?;

        let parts_full = cache_dir.join(&info.dir_path);
        if parts_full.is_dir() {
            let _ = std::fs::remove_dir_all(&parts_full);
        }

        let fp = final_path.to_string_lossy().to_string();
        let rid = resource_id;
        self.conn
            .call(move |conn| {
                conn.execute(
                    "UPDATE range_resources SET is_complete = TRUE, assembled_path = ?1 WHERE id = ?2",
                    params![fp, rid],
                )?;
                conn.execute(
                    "DELETE FROM range_slabs WHERE resource_id = ?1",
                    params![rid],
                )?;
                Ok(())
            })
            .await
            .map_err(|e| Error::Proxy(format!("Failed to update assembly status: {}", e)))?;

        let now = now_unix();
        let file_size = assembled.len() as i64;
        let fp_str = final_path.to_string_lossy().to_string();
        let fingerprint = crate::cache::key::compute_fingerprint("GET", url, &[]);
        let url = url.to_string();
        let ct = content_type.map(|s| s.to_string());
        let h = host.to_string();
        let mt = ct.as_deref().and_then(store::classify_media_type).map(|s| s.to_string());

        let index = crate::cache::index::CacheIndex::from_conn(self.conn.clone());
        let entry = crate::cache::index::CacheEntry {
            fingerprint,
            url,
            method: "GET".into(),
            status_code: 200,
            content_type: ct,
            content_length: Some(file_size),
            response_headers: "{}".into(),
            cache_policy: vec![],
            created_at: now,
            last_accessed: now,
            expires_at: None,
            file_path: fp_str,
            file_size,
            host: h,
            vary_key: None,
            media_type: mt,
            status: "active".into(),
            stale_at: None,
        };
        index.insert(&entry).await?;

        tracing::info!(
            path = %final_path.display(),
            size = assembled.len(),
            "Assembled complete file from range slabs"
        );

        Ok(final_path)
    }

    /// Write the manifest JSON file in the .parts/ directory.
    async fn write_manifest(&self, resource_id: i64, dir_path: &Path) -> Result<(), Error> {
        let rid = resource_id;
        let cache_dir = self.cache_dir.clone();
        let dp = dir_path.to_path_buf();

        self.conn
            .call(move |conn| {
                let (url, total_size, ct): (String, Option<i64>, Option<String>) = conn.query_row(
                    "SELECT url_pattern, total_size, content_type FROM range_resources WHERE id = ?1",
                    params![rid],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )?;

                let mut stmt = conn.prepare(
                    "SELECT range_start, range_end FROM range_slabs WHERE resource_id = ?1 ORDER BY range_start",
                )?;
                let slabs: Vec<SlabInfo> = stmt
                    .query_map(params![rid], |row| {
                        Ok(SlabInfo {
                            start: row.get::<_, i64>(0)? as u64,
                            end: row.get::<_, i64>(1)? as u64,
                        })
                    })?
                    .collect::<Result<Vec<_>, _>>()?;

                let coverage_bytes: u64 = slabs.iter().map(|s| s.end - s.start + 1).sum();
                let coverage_pct = total_size.map(|t| (coverage_bytes as f64 / t as f64) * 100.0);

                let manifest = RangeManifest {
                    url,
                    total_size: total_size.map(|t| t as u64),
                    content_type: ct,
                    slabs,
                    coverage_bytes,
                    coverage_pct,
                };

                let json = serde_json::to_string_pretty(&manifest)
                    .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
                let manifest_path = cache_dir.join(dp.join("_manifest.json"));
                if let Some(parent) = manifest_path.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                std::fs::write(&manifest_path, json)
                    .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;

                Ok(())
            })
            .await
            .map_err(|e| Error::Proxy(format!("Failed to write manifest: {}", e)))
    }
}

/// Info needed to assemble a complete file from slabs.
#[derive(Debug)]
pub struct AssemblyInfo {
    pub dir_path: PathBuf,
    pub total_size: u64,
    pub slabs: Vec<(u64, u64, PathBuf)>,
}

/// YouTube URL normalization.
pub mod youtube {
    #[allow(dead_code)]
    const EPHEMERAL_PARAMS: &[&str] = &[
        "expire", "ei", "ip", "aitags", "source", "requiressl",
        "xpc", "vprv", "svpuc", "mime", "ns", "gir", "clen", "ratebypass",
        "dur", "lmt", "fexp", "c", "txp", "n", "sparams", "sig", "signature",
        "lsparams", "lsig", "mh", "mm", "mn", "ms", "mv", "mvi", "pl",
        "gcr", "initcwndbps", "spc", "sn", "cpn", "cver", "ump",
    ];

    const STABLE_PARAMS: &[&str] = &["id", "video_id", "itag", "clen"];

    pub fn is_youtube_videoplayback(url: &str) -> bool {
        url.contains("/videoplayback") && url.contains("itag=")
    }

    pub fn normalize(url: &str) -> Option<(String, Option<String>, Option<(u64, u64)>)> {
        let parsed = url::Url::parse(url).ok()?;

        if !parsed.path().contains("videoplayback") {
            return None;
        }

        let params: std::collections::HashMap<String, String> = parsed
            .query_pairs()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();

        let itag = params.get("itag")?.clone();

        let range = params.get("range").and_then(|r| {
            let (s, e) = r.split_once('-')?;
            Some((s.parse().ok()?, e.parse().ok()?))
        });

        let host = parsed.host_str()?;
        let mut stable: Vec<(&str, &str)> = Vec::new();
        for key in STABLE_PARAMS {
            if let Some(val) = params.get(*key) {
                stable.push((key, val));
            }
        }
        stable.sort_by_key(|(k, _)| *k);

        let query: String = stable
            .iter()
            .map(|(k, v)| format!("{}={}", k, v))
            .collect::<Vec<_>>()
            .join("&");

        let normalized = format!("https://{}/videoplayback?{}", host, query);

        Some((normalized, Some(itag), range))
    }

    pub fn itag_to_ext(itag: &str) -> &'static str {
        match itag {
            "242" | "243" | "244" | "247" | "248" | "271" | "313" => ".mp4",
            "394" | "395" | "396" | "397" | "398" | "399" | "400" | "401" => ".mp4",
            "139" | "140" | "141" => ".m4a",
            "171" | "249" | "250" | "251" => ".webm",
            "18" | "22" | "37" | "38" => ".mp4",
            "43" | "44" | "45" | "46" => ".webm",
            _ => ".bin",
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn test_is_youtube() {
            assert!(is_youtube_videoplayback(
                "https://rr5---sn-abc.googlevideo.com/videoplayback?expire=123&itag=247&range=0-65535"
            ));
            assert!(!is_youtube_videoplayback("https://example.com/video.mp4"));
        }

        #[test]
        fn test_normalize() {
            let url = "https://rr5---sn-abc.googlevideo.com/videoplayback?expire=123&ei=xxx&itag=247&id=abc&range=0-65535&sig=yyy&clen=1000";
            let (normalized, itag, range) = normalize(url).unwrap();

            assert!(normalized.contains("itag=247"));
            assert!(normalized.contains("id=abc"));
            assert!(normalized.contains("clen=1000"));
            assert!(!normalized.contains("expire="));
            assert!(!normalized.contains("sig="));
            assert_eq!(itag, Some("247".to_string()));
            assert_eq!(range, Some((0, 65535)));
        }

        #[test]
        fn test_itag_to_ext() {
            assert_eq!(itag_to_ext("247"), ".mp4");
            assert_eq!(itag_to_ext("251"), ".webm");
            assert_eq!(itag_to_ext("140"), ".m4a");
        }
    }
}

fn build_parts_dir(url: &str, content_type: Option<&str>) -> PathBuf {
    let sanitized = store::url_to_cache_path("GET", url, content_type, None);
    let path_str = sanitized.to_string_lossy();
    PathBuf::from(format!("{}.parts", path_str))
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_content_range() {
        let cr = ContentRange::parse("bytes 0-1023/4096").unwrap();
        assert_eq!(cr.start, 0);
        assert_eq!(cr.end, 1023);
        assert_eq!(cr.total, Some(4096));

        let cr = ContentRange::parse("bytes 1024-2047/*").unwrap();
        assert_eq!(cr.start, 1024);
        assert_eq!(cr.end, 2047);
        assert_eq!(cr.total, None);
    }

    #[test]
    fn test_parse_range_header() {
        assert_eq!(parse_range_header("bytes=0-1023"), Some((0, Some(1023))));
        assert_eq!(parse_range_header("bytes=1024-"), Some((1024, None)));
    }
}
