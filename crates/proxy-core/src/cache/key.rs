use sha2::{Digest, Sha256};

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
}
