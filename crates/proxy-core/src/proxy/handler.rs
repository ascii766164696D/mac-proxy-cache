use http::header;
use http_body_util::{BodyExt, Full, StreamBody};
use hudsucker::hyper::body::Frame;
use hudsucker::hyper::{Request, Response};
use hudsucker::{Body, HttpContext, HttpHandler, RequestOrResponse};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

/// Domains that use certificate pinning — don't MITM intercept these.
/// Keep this list minimal — only domains that actively reject MITM certs.
const PASSTHROUGH_DOMAINS: &[&str] = &[
    // Apple system services (hard pinning)
    "mesu.apple.com",
    "xp.apple.com",
    "gdmf.apple.com",
    "configuration.apple.com",
    // OCSP stapling
    "ocsp.apple.com",
    "ocsp2.apple.com",
];

use std::collections::VecDeque;
use std::sync::Mutex;

use crate::cache::index::{CacheEntry, CacheIndex};
use crate::cache::key;
use crate::cache::policy::CachedPolicy;
use crate::cache::range::{self, ContentRange, RangeCache, SlabHit};
use crate::cache::store;

/// A logged request/response for the live monitor.
#[derive(Debug, Clone, serde::Serialize)]
pub struct RequestLogEntry {
    pub id: u64,
    pub timestamp: i64,
    pub method: String,
    pub url: String,
    pub status_code: u16,
    pub size: i64,
    pub from_cache: bool,
    pub content_type: Option<String>,
    pub host: String,
    /// Relative path in cache dir, if cached on disk
    pub file_path: Option<String>,
}

/// Shared state across all handler clones.
pub struct ProxyState {
    pub cache_index: CacheIndex,
    pub range_cache: RangeCache,
    pub cache_dir: PathBuf,
    pub bypass: AtomicBool,
    pub max_cache_size: AtomicU64,
    pub max_entry_size: AtomicU64,
    pub serve_stale_on_error: bool,
    pub system_proxy_enabled: AtomicBool,
    pub proxy_port: u16,
    pub requests: AtomicU64,
    pub cache_hits: AtomicU64,
    pub cache_misses: AtomicU64,
    pub bytes_saved: AtomicU64,
    pub touch_tx: mpsc::UnboundedSender<(String, i64)>,
    pub request_log: Mutex<VecDeque<RequestLogEntry>>,
    pub request_log_counter: AtomicU64,
}

impl ProxyState {
    pub fn log_request(&self, entry: RequestLogEntry) {
        if let Ok(mut log) = self.request_log.lock() {
            log.push_back(entry);
            while log.len() > 2000 {
                log.pop_front();
            }
        }
    }

    pub fn update_request_size(&self, id: u64, size: i64) {
        if let Ok(mut log) = self.request_log.lock() {
            if let Some(entry) = log.iter_mut().rev().find(|e| e.id == id) {
                entry.size = size;
            }
        }
    }

    pub fn update_request_cache_hit(&self, id: u64, size: i64, status_code: u16, file_path: Option<String>) {
        if let Ok(mut log) = self.request_log.lock() {
            if let Some(entry) = log.iter_mut().rev().find(|e| e.id == id) {
                entry.from_cache = true;
                entry.size = size;
                entry.status_code = status_code;
                entry.file_path = file_path;
            }
        }
    }

    pub fn get_requests_since(&self, since_id: u64) -> Vec<RequestLogEntry> {
        if let Ok(log) = self.request_log.lock() {
            log.iter().filter(|e| e.id > since_id).cloned().collect()
        } else {
            Vec::new()
        }
    }

    pub fn stats(&self) -> ProxyStats {
        ProxyStats {
            requests: self.requests.load(Ordering::Relaxed),
            cache_hits: self.cache_hits.load(Ordering::Relaxed),
            cache_misses: self.cache_misses.load(Ordering::Relaxed),
            bytes_saved: self.bytes_saved.load(Ordering::Relaxed),
            bypass_enabled: self.bypass.load(Ordering::Relaxed),
            system_proxy_enabled: self.system_proxy_enabled.load(Ordering::Relaxed),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ProxyStats {
    pub requests: u64,
    pub cache_hits: u64,
    pub cache_misses: u64,
    pub bytes_saved: u64,
    pub bypass_enabled: bool,
    pub system_proxy_enabled: bool,
}

/// Per-request context saved between handle_request and handle_response.
#[derive(Clone, Default)]
struct RequestContext {
    uri: String,
    method: String,
    fingerprint: String,
    should_cache: bool,
    existing_entry: Option<CacheEntry>,
    /// Range request info (if Range header was present)
    range_start: Option<u64>,
    range_end: Option<u64>,
    /// Normalized URL for YouTube or range resources
    normalized_url: Option<String>,
}

/// HTTP handler with caching logic.
#[derive(Clone)]
pub struct CachingHandler {
    pub state: Arc<ProxyState>,
    req_ctx: RequestContext,
}

impl CachingHandler {
    pub fn new(state: Arc<ProxyState>) -> Self {
        Self {
            state,
            req_ctx: RequestContext::default(),
        }
    }
}

impl HttpHandler for CachingHandler {
    async fn handle_request(
        &mut self,
        _ctx: &HttpContext,
        req: Request<Body>,
    ) -> RequestOrResponse {
        self.state.requests.fetch_add(1, Ordering::Relaxed);

        let method = req.method().to_string();
        let uri = req.uri().to_string();

        self.req_ctx = RequestContext {
            uri: uri.clone(),
            method: method.clone(),
            fingerprint: String::new(),
            should_cache: false,
            existing_entry: None,
            range_start: None,
            range_end: None,
            normalized_url: None,
        };

        // Only cache GET requests; forward everything else
        if method != "GET" {
            return req.into();
        }

        // Check for YouTube URL normalization
        let effective_uri;
        if let Some((normalized, _itag, yt_range)) = range::youtube::normalize(&uri) {
            self.req_ctx.normalized_url = Some(normalized.clone());
            if let Some((start, end)) = yt_range {
                self.req_ctx.range_start = Some(start);
                self.req_ctx.range_end = Some(end);
            }
            effective_uri = normalized;
        } else {
            effective_uri = uri.clone();
        }

        // Detect Range header
        if let Some(range_hdr) = req.headers().get(header::RANGE).and_then(|v| v.to_str().ok()) {
            if let Some((start, end)) = range::parse_range_header(range_hdr) {
                self.req_ctx.range_start = Some(start);
                self.req_ctx.range_end = end;
            }
        }

        // Normalize URL for cache key (strips cosmetic query params from static resources)
        let cache_uri = key::normalize_url(&effective_uri);
        let fingerprint = key::compute_fingerprint(&method, &cache_uri, &[]);
        self.req_ctx.fingerprint = fingerprint.clone();
        self.req_ctx.should_cache = true;

        // Bypass mode: forward but still cache in handle_response
        if self.state.bypass.load(Ordering::Relaxed) {
            return req.into();
        }

        // Client no-cache / max-age=0: don't serve from cache directly,
        // but still look up cached entry for revalidation headers (If-None-Match etc.)
        let no_cache = req
            .headers()
            .get(header::CACHE_CONTROL)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.contains("no-cache") || v.contains("max-age=0"));

        // For range requests, check if we have a slab that fully covers the requested range
        if let Some(start) = self.req_ctx.range_start {
            let url_pattern = self.req_ctx.normalized_url.as_deref().unwrap_or(&uri);
            if let Ok(Some(hit)) = self.state.range_cache.find_covering_slab(
                url_pattern,
                start,
                self.req_ctx.range_end,
            ).await {
                if let Ok(body) = store::read_body(&self.state.cache_dir, &hit.slab_path) {
                    let offset = (hit.serve_start - hit.slab_start) as usize;
                    let len = (hit.serve_end - hit.serve_start + 1) as usize;

                    if offset + len <= body.len() {
                        let slice = &body[offset..offset + len];
                        let content_range = if let Some(total) = hit.total {
                            format!("bytes {}-{}/{}", hit.serve_start, hit.serve_end, total)
                        } else {
                            format!("bytes {}-{}/*", hit.serve_start, hit.serve_end)
                        };

                        tracing::info!(uri = %uri, range = %content_range, "Range cache HIT");
                        self.state.cache_hits.fetch_add(1, Ordering::Relaxed);
                        self.state.bytes_saved.fetch_add(len as u64, Ordering::Relaxed);

                        let response = Response::builder()
                            .status(206)
                            .header(header::CONTENT_RANGE, &content_range)
                            .header(header::CONTENT_LENGTH, len.to_string())
                            .header("X-Cache", "HIT")
                            .body(Body::from(Full::new(bytes::Bytes::copy_from_slice(slice))))
                            .unwrap();
                        return response.into();
                    }
                }
            }
            // No covering slab found — forward to upstream
        }

        // Try cache lookup
        if let Ok(Some(entry)) = self.state.cache_index.lookup(&fingerprint).await {
            // Skip cache if response Varies on Origin (CORS-sensitive)
            let varies_on_origin = entry
                .response_headers
                .contains("\"vary\"")
                && entry.response_headers.to_lowercase().contains("origin");

            if let Ok(policy) = CachedPolicy::from_bytes(&entry.cache_policy) {
                if policy.is_fresh() && !varies_on_origin && !no_cache {
                    // Fresh hit — serve from cache
                    tracing::info!(uri = %uri, "Cache HIT");
                    self.state.cache_hits.fetch_add(1, Ordering::Relaxed);
                    self.state
                        .bytes_saved
                        .fetch_add(entry.file_size as u64, Ordering::Relaxed);
                    let _ = self.state.touch_tx.send((fingerprint, now_unix()));

                    match serve_from_cache(&self.state.cache_dir, &entry) {
                        Ok(response) => {
                            self.state.log_request(RequestLogEntry {
                                id: self.state.request_log_counter.fetch_add(1, Ordering::Relaxed),
                                timestamp: now_unix(),
                                method: method.clone(),
                                url: uri.clone(),
                                status_code: entry.status_code,
                                size: entry.file_size,
                                from_cache: true,
                                content_type: entry.content_type.clone(),
                                host: entry.host.clone(),
                                file_path: Some(entry.file_path.clone()),
                            });
                            return response.into();
                        }
                        Err(e) => tracing::warn!(uri = %uri, "Cache read failed: {}", e),
                    }
                }
            }

            // Stale or no-cache — add revalidation headers and store entry for 304 handling
            tracing::debug!(uri = %uri, "Cache stale, revalidating");
            self.req_ctx.existing_entry = Some(entry.clone());

            let headers: serde_json::Value =
                serde_json::from_str(&entry.response_headers).unwrap_or_default();

            let mut req = req;
            if let Some(etag) = headers.get("etag").and_then(|v| v.as_str()) {
                if let Ok(val) = etag.parse() {
                    req.headers_mut().insert(header::IF_NONE_MATCH, val);
                }
            }
            if let Some(lm) = headers.get("last-modified").and_then(|v| v.as_str()) {
                if let Ok(val) = lm.parse() {
                    req.headers_mut().insert(header::IF_MODIFIED_SINCE, val);
                }
            }
            return req.into();
        }

        self.state.cache_misses.fetch_add(1, Ordering::Relaxed);
        req.into()
    }

    /// Don't intercept WebSocket hosts and cert-pinning domains.
    /// Called on CONNECT requests before MITM — if we return false,
    /// the connection is tunneled directly without decryption.
    async fn should_intercept(
        &mut self,
        _ctx: &HttpContext,
        req: &Request<Body>,
    ) -> bool {
        let host = req.uri().host().unwrap_or("");

        // Known WebSocket-only hosts
        if host.starts_with("alive.") || host.contains("-realtime") || host.contains("-ws") {
            return false;
        }

        // Don't intercept IP-based connections (cert pinning)
        if host.parse::<std::net::IpAddr>().is_ok() {
            return false;
        }

        // Apple services (hard cert pinning)
        if host.ends_with(".apple.com")
            || host.ends_with(".icloud.com")
            || host.ends_with(".mzstatic.com")
            || host.ends_with(".itunes.apple.com")
        {
            return false;
        }


        true
    }

    async fn handle_response(
        &mut self,
        _ctx: &HttpContext,
        res: Response<Body>,
    ) -> Response<Body> {
        let method = &self.req_ctx.method;
        let uri = self.req_ctx.uri.clone();
        let status = res.status();

        // Log every response to the request monitor
        let response_ct = res.headers().get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok()).map(|s| s.to_string());
        let response_cl = res.headers().get(header::CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok()).and_then(|v| v.parse::<i64>().ok()).unwrap_or(0);
        let host = url::Url::parse(&uri).ok()
            .and_then(|u| u.host_str().map(|s| s.to_string()))
            .unwrap_or_default();
        // Check if this URL is already cached on disk
        let cached_file = if self.req_ctx.should_cache {
            self.state.cache_index.lookup(&self.req_ctx.fingerprint).await
                .ok().flatten().map(|e| e.file_path)
        } else {
            None
        };
        let log_id = self.state.request_log_counter.fetch_add(1, Ordering::Relaxed);
        self.state.log_request(RequestLogEntry {
            id: log_id,
            timestamp: now_unix(),
            method: method.clone(),
            url: uri.clone(),
            status_code: status.as_u16(),
            size: response_cl,
            from_cache: false,
            content_type: response_ct,
            host,
            file_path: cached_file,
        });

        // Helper: wrap response to count body size if Content-Length was missing
        let needs_size = response_cl == 0;
        let state_for_size = self.state.clone();
        let maybe_count = |res: Response<Body>| -> Response<Body> {
            if needs_size { with_size_counting(state_for_size.clone(), log_id, res) } else { res }
        };

        // Handle unsafe method invalidation (POST/PUT/DELETE)
        if matches!(method.as_str(), "POST" | "PUT" | "DELETE" | "PATCH") && status.is_success() {
            self.invalidate_for_unsafe_method(&uri, &res).await;
            return maybe_count(res);
        }

        if !self.req_ctx.should_cache {
            return maybe_count(res);
        }

        let fingerprint = self.req_ctx.fingerprint.clone();

        // Handle 304 Not Modified — update policy, serve from cache
        if status == http::StatusCode::NOT_MODIFIED {
            if let Some(ref entry) = self.req_ctx.existing_entry {
                tracing::info!(uri = %uri, "304 Not Modified, serving from cache");
                self.state.cache_hits.fetch_add(1, Ordering::Relaxed);
                self.state
                    .bytes_saved
                    .fetch_add(entry.file_size as u64, Ordering::Relaxed);

                // Merge 304 headers into existing stored headers.
                // A 304 may only include a subset of headers (freshness-related).
                // We must keep the original body-related headers (content-type,
                // content-encoding, etc.) since the cached body hasn't changed.
                let merged = merge_304_headers(&entry.response_headers, &res);
                let policy_bytes = build_cache_policy_bytes(&uri, &merged);
                let _ = self
                    .state
                    .cache_index
                    .update_policy(&fingerprint, policy_bytes, &merged)
                    .await;

                match serve_from_cache(&self.state.cache_dir, entry) {
                    Ok(response) => {
                        self.state.update_request_cache_hit(
                            log_id, entry.file_size, entry.status_code, Some(entry.file_path.clone()),
                        );
                        return response;
                    }
                    Err(e) => tracing::warn!(uri = %uri, "Cache read failed after 304: {}", e),
                }
            }
            return maybe_count(res);
        }

        // Handle 206 Partial Content — store as range slab
        if status == http::StatusCode::PARTIAL_CONTENT {
            return self.handle_206_response(res).await;
        }

        // Handle 404/410 — mark existing cached entry as stale
        if status == http::StatusCode::NOT_FOUND || status == http::StatusCode::GONE {
            if let Some(ref entry) = self.req_ctx.existing_entry {
                tracing::info!(uri = %uri, status = %status, "Origin returned error, marking cache stale");
                self.mark_entry_stale(entry).await;
            }
            return maybe_count(res);
        }

        // Only cache successful responses
        if !status.is_success() {
            return maybe_count(res);
        }

        // Check cache-control headers
        // Note: we only skip `no-store`. We intentionally cache `private` responses
        // because this is a personal proxy, not a shared/public one.
        if let Some(cc) = res
            .headers()
            .get(header::CACHE_CONTROL)
            .and_then(|v| v.to_str().ok())
        {
            if cc.contains("no-store") {
                return maybe_count(res);
            }
        }

        // Large response guard: skip caching if Content-Length exceeds max_entry_size
        if let Some(cl) = res
            .headers()
            .get(header::CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<u64>().ok())
        {
            let max_entry = self.state.max_entry_size.load(Ordering::Relaxed);
            if cl > max_entry {
                tracing::debug!(uri = %uri, size = cl, max = max_entry, "Response too large, skipping cache");
                return maybe_count(res);
            }
        }

        // Don't cache HTML pages — they're often SPAs with dynamic/auth content.
        // Only cache static assets (JS, CSS, images, fonts, media, JSON APIs with cache headers).
        if let Some(ct) = res.headers().get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()) {
            if ct.contains("text/html") {
                return maybe_count(res);
            }
        }

        // Don't cache CORS-sensitive responses:
        // 1. Responses that Vary on Origin
        // 2. Responses with a specific Access-Control-Allow-Origin (not "*")
        // These would break when served to a different origin.
        if let Some(vary) = res.headers().get(header::VARY).and_then(|v| v.to_str().ok()) {
            if vary.to_lowercase().contains("origin") {
                return maybe_count(res);
            }
        }
        if let Some(acao) = res.headers().get(header::ACCESS_CONTROL_ALLOW_ORIGIN).and_then(|v| v.to_str().ok()) {
            if acao != "*" {
                return maybe_count(res);
            }
        }

        // Track content-encoding for decompressing when writing to disk cache
        let content_encoding = res
            .headers()
            .get(header::CONTENT_ENCODING)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());

        // Cache the new response via streaming tee — don't block the client
        let content_type = res
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());

        let headers_json = extract_headers_json(&res);
        let host = url::Url::parse(&uri)
            .ok()
            .and_then(|u| u.host_str().map(|s| s.to_string()))
            .unwrap_or_else(|| "_unknown_".to_string());

        let media_type = content_type
            .as_deref()
            .and_then(store::classify_media_type)
            .map(|s| s.to_string());

        let cache_path = store::url_to_cache_path("GET", &uri, content_type.as_deref(), None);
        let status_code = status.as_u16();
        let existing = self.req_ctx.existing_entry.take();

        let (parts, body) = res.into_parts();

        // Create a channel to stream the body to the client
        let (tx, rx) =
            mpsc::channel::<Result<Frame<bytes::Bytes>, hudsucker::Error>>(32);

        let state = self.state.clone();
        let fp = fingerprint;
        let u = uri;
        let ct = content_type;
        let hj = headers_json;
        let cp = cache_path;
        let h = host;
        let mt = media_type;
        let ce = content_encoding;

        // Spawn streaming tee task: reads body, forwards to client, buffers for cache
        tokio::spawn(async move {
            let mut cache_buf = Vec::new();
            let mut body = body;

            // Timeout the entire body read to prevent connection leaks
            let tee_result = tokio::time::timeout(
                std::time::Duration::from_secs(300), // 5 min max per response
                async {
                    loop {
                        match body.frame().await {
                            Some(Ok(frame)) => {
                                if let Some(data) = frame.data_ref() {
                                    cache_buf.extend_from_slice(data);
                                }
                                let send_result: Result<Frame<bytes::Bytes>, hudsucker::Error> = Ok(frame);
                                if tx.send(send_result).await.is_err() {
                                    return false; // Client disconnected
                                }
                            }
                            Some(Err(e)) => {
                                let _ = tx.send(Err(e)).await;
                                return false;
                            }
                            None => return true, // Body complete
                        }
                    }
                }
            ).await;

            let body_complete = matches!(tee_result, Ok(true));

            // Update the log entry with actual body size
            if !cache_buf.is_empty() {
                state.update_request_size(log_id, cache_buf.len() as i64);
            }

            // Drop sender to signal end of stream to client
            drop(tx);

            if !body_complete {
                // Timed out or client disconnected — don't cache partial body
                return;
            }

            // Now cache the buffered body
            if cache_buf.is_empty() {
                return;
            }

            let file_size = cache_buf.len() as i64;

            // Mark old entry stale if replacing
            if let Some(ref old) = existing {
                let old_path = PathBuf::from(&old.file_path);
                if let Ok(stale_path) = store::rename_to_stale(&state.cache_dir, &old_path) {
                    let _ = state
                        .cache_index
                        .mark_stale(&old.fingerprint, &stale_path.to_string_lossy())
                        .await;
                }
            }

            // Decompress for disk storage so files are viewable in Finder.
            // The client already got the original (possibly compressed) body via the tee.
            let (cache_buf, was_decompressed) = decompress_body(cache_buf, ce.as_deref());

            // Strip encoding headers from stored JSON if we decompressed,
            // so cache HITs serve decompressed content without encoding header.
            let hj = if was_decompressed {
                strip_encoding_headers(&hj)
            } else {
                hj
            };

            // Write to disk
            if let Err(e) = store::write_body(&state.cache_dir, &cp, &cache_buf) {
                tracing::warn!(url = %u, "Cache write failed: {}", e);
                return;
            }

            let now = now_unix();
            let policy_bytes = build_cache_policy_bytes(&u, &hj);

            let entry = CacheEntry {
                fingerprint: fp,
                url: u.clone(),
                method: "GET".into(),
                status_code,
                content_type: ct,
                content_length: Some(file_size),
                response_headers: hj,
                cache_policy: policy_bytes,
                created_at: now,
                last_accessed: now,
                expires_at: None,
                file_path: cp.to_string_lossy().to_string(),
                file_size,
                host: h,
                vary_key: None,
                media_type: mt,
                status: "active".into(),
                stale_at: None,
            };

            if let Err(e) = state.cache_index.insert(&entry).await {
                tracing::warn!(url = %u, "Cache index insert failed: {}", e);
            } else {
                tracing::info!(url = %u, path = %cp.display(), size = file_size, "Cached response");
            }
        });

        // Return response immediately with streaming body — client gets data as it arrives
        let stream = ReceiverStream::new(rx);
        let stream_body = StreamBody::new(stream);
        Response::from_parts(parts, Body::from(stream_body))
    }
}

impl CachingHandler {
    /// Handle a 206 Partial Content response — store as a range slab.
    async fn handle_206_response(&self, res: Response<Body>) -> Response<Body> {
        let uri = self.req_ctx.uri.clone();
        let url_pattern = self.req_ctx.normalized_url.as_deref().unwrap_or(&uri);

        // Parse Content-Range header
        let content_range = res
            .headers()
            .get(header::CONTENT_RANGE)
            .and_then(|v| v.to_str().ok())
            .and_then(ContentRange::parse);

        let Some(cr) = content_range else {
            tracing::debug!(uri = %uri, "206 without valid Content-Range, skipping");
            return res;
        };

        let content_type = res
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());

        let host = url::Url::parse(&uri)
            .ok()
            .and_then(|u| u.host_str().map(|s| s.to_string()))
            .unwrap_or_else(|| "_unknown_".to_string());

        // Collect the body
        let (parts, body) = res.into_parts();

        match body.collect().await {
            Ok(collected) => {
                let body_bytes = collected.to_bytes();
                let body_vec = body_bytes.to_vec();

                let state = self.state.clone();
                let url_pat = url_pattern.to_string();
                let h = host;
                let ct = content_type;
                let cr2 = cr.clone();

                tokio::spawn(async move {
                    // Get or create the range resource
                    let (resource_id, dir_path) = match state
                        .range_cache
                        .get_or_create_resource(&url_pat, &h, ct.as_deref())
                        .await
                    {
                        Ok(v) => v,
                        Err(e) => {
                            tracing::warn!(url = %url_pat, "Failed to create range resource: {}", e);
                            return;
                        }
                    };

                    // Store the slab
                    if let Err(e) = state
                        .range_cache
                        .store_slab(resource_id, &dir_path, &cr2, &body_vec)
                        .await
                    {
                        tracing::warn!(url = %url_pat, "Failed to store range slab: {}", e);
                        return;
                    }

                    tracing::info!(
                        url = %url_pat,
                        range = format!("{}-{}", cr2.start, cr2.end),
                        total = ?cr2.total,
                        "Stored range slab"
                    );

                    // Check if assembly is ready
                    if let Ok(Some(info)) = state.range_cache.check_assembly_ready(resource_id).await {
                        tracing::info!(url = %url_pat, "All slabs present, assembling...");
                        if let Err(e) = state
                            .range_cache
                            .assemble(resource_id, info, &url_pat, ct.as_deref(), &h)
                            .await
                        {
                            tracing::warn!(url = %url_pat, "Assembly failed: {}", e);
                        }
                    }
                });

                Response::from_parts(parts, Body::from(Full::new(body_bytes)))
            }
            Err(e) => {
                tracing::warn!(uri = %uri, "Failed to collect 206 body: {}", e);
                Response::from_parts(parts, Body::empty())
            }
        }
    }

    /// Mark an existing cache entry as stale (rename file on disk).
    async fn mark_entry_stale(&self, entry: &CacheEntry) {
        let old_path = PathBuf::from(&entry.file_path);
        match store::rename_to_stale(&self.state.cache_dir, &old_path) {
            Ok(stale_path) => {
                let _ = self
                    .state
                    .cache_index
                    .mark_stale(&entry.fingerprint, &stale_path.to_string_lossy())
                    .await;
                tracing::info!(
                    path = %old_path.display(),
                    "Marked cache entry as stale"
                );
            }
            Err(e) => tracing::warn!("Failed to mark entry stale: {}", e),
        }
    }

    /// Invalidate cached GET entries after a successful unsafe method response.
    async fn invalidate_for_unsafe_method(&self, uri: &str, res: &Response<Body>) {
        // Invalidate the request URL itself
        if let Ok(entries) = self.state.cache_index.invalidate_by_url(uri).await {
            for (_, file_path) in &entries {
                let path = PathBuf::from(file_path);
                let _ = store::rename_to_stale(&self.state.cache_dir, &path);
            }
            if !entries.is_empty() {
                tracing::info!(uri = %uri, count = entries.len(), "Invalidated cache entries for unsafe method");
            }
        }

        // Also invalidate URLs from Location and Content-Location headers
        for header_name in &[header::LOCATION, header::CONTENT_LOCATION] {
            if let Some(loc) = res.headers().get(header_name).and_then(|v| v.to_str().ok()) {
                // Resolve relative URLs
                let loc_url = if loc.starts_with("http") {
                    loc.to_string()
                } else if let Ok(base) = url::Url::parse(uri) {
                    base.join(loc).map(|u| u.to_string()).unwrap_or_default()
                } else {
                    continue;
                };

                if !loc_url.is_empty() {
                    if let Ok(entries) = self.state.cache_index.invalidate_by_url(&loc_url).await {
                        for (_, file_path) in &entries {
                            let path = PathBuf::from(file_path);
                            let _ = store::rename_to_stale(&self.state.cache_dir, &path);
                        }
                    }
                }
            }
        }
    }
}

fn serve_from_cache(
    cache_dir: &PathBuf,
    entry: &CacheEntry,
) -> Result<Response<Body>, crate::error::Error> {
    let body = store::read_body(cache_dir, &PathBuf::from(&entry.file_path))?;

    let mut builder = Response::builder().status(entry.status_code);

    if let Ok(headers) = serde_json::from_str::<serde_json::Value>(&entry.response_headers) {
        if let Some(obj) = headers.as_object() {
            for (name, value) in obj {
                if let Some(v) = value.as_str() {
                    if let (Ok(name), Ok(val)) = (
                        http::header::HeaderName::from_bytes(name.as_bytes()),
                        http::header::HeaderValue::from_bytes(v.as_bytes()),
                    ) {
                        builder = builder.header(name, val);
                    }
                }
            }
        }
    }

    builder = builder.header("X-Cache", "HIT");

    let response = builder
        .body(Body::from(Full::new(bytes::Bytes::from(body))))
        .map_err(|e| {
            crate::error::Error::Proxy(format!("Failed to build cached response: {}", e))
        })?;

    Ok(response)
}

fn extract_headers_json(res: &Response<Body>) -> String {
    let mut header_map = serde_json::Map::new();
    for (name, value) in res.headers() {
        // Use to_str() for visible ASCII, fall back to lossy for binary headers
        let v = match value.to_str() {
            Ok(s) => s.to_string(),
            Err(_) => String::from_utf8_lossy(value.as_bytes()).to_string(),
        };
        header_map.insert(
            name.as_str().to_string(),
            serde_json::Value::String(v),
        );
    }
    serde_json::Value::Object(header_map).to_string()
}

fn build_cache_policy_bytes(url: &str, headers_json: &str) -> Vec<u8> {
    use http_cache_semantics::CachePolicy;

    let req = http::Request::builder()
        .method("GET")
        .uri(url)
        .body(())
        .unwrap_or_else(|_| http::Request::new(()));

    let mut res_builder = http::Response::builder().status(200);
    if let Ok(headers) = serde_json::from_str::<serde_json::Value>(headers_json) {
        if let Some(obj) = headers.as_object() {
            for (name, value) in obj {
                if let Some(v) = value.as_str() {
                    if let (Ok(n), Ok(val)) = (
                        http::header::HeaderName::from_bytes(name.as_bytes()),
                        http::header::HeaderValue::from_str(v),
                    ) {
                        res_builder = res_builder.header(n, val);
                    }
                }
            }
        }
    }

    let res = res_builder
        .body(())
        .unwrap_or_else(|_| http::Response::new(()));
    let policy = CachePolicy::new(&req, &res);
    serde_json::to_vec(&policy).unwrap_or_default()
}

/// Merge headers from a 304 response into existing stored headers.
/// Only updates freshness/validation headers; preserves body-related headers
/// (content-type, content-encoding, content-length) from the original response.
fn merge_304_headers(existing_json: &str, res_304: &Response<Body>) -> String {
    // Headers that a 304 can legitimately update (RFC 7232 §4.1)
    const UPDATABLE: &[&str] = &[
        "cache-control",
        "date",
        "etag",
        "expires",
        "last-modified",
        "vary",
        "set-cookie",
        "strict-transport-security",
        "x-github-request-id",
    ];

    let mut stored: serde_json::Map<String, serde_json::Value> =
        serde_json::from_str(existing_json).unwrap_or_default();

    for (name, value) in res_304.headers() {
        let key = name.as_str().to_lowercase();
        if UPDATABLE.contains(&key.as_str()) {
            let v = match value.to_str() {
                Ok(s) => s.to_string(),
                Err(_) => String::from_utf8_lossy(value.as_bytes()).to_string(),
            };
            stored.insert(key, serde_json::Value::String(v));
        }
    }

    serde_json::to_string(&serde_json::Value::Object(stored))
        .unwrap_or_else(|_| existing_json.to_string())
}

/// Strip content-encoding and content-length from stored headers JSON.
/// Called after decompression so cached responses don't claim to be compressed.
fn strip_encoding_headers(headers_json: &str) -> String {
    if let Ok(mut headers) = serde_json::from_str::<serde_json::Value>(headers_json) {
        if let Some(obj) = headers.as_object_mut() {
            obj.remove("content-encoding");
            obj.remove("content-length");
            obj.remove("transfer-encoding");
        }
        serde_json::to_string(&headers).unwrap_or_else(|_| headers_json.to_string())
    } else {
        headers_json.to_string()
    }
}

/// Decompress body bytes based on Content-Encoding.
/// Returns (body, was_decompressed). If decompression fails or encoding
/// is unsupported, returns original bytes with was_decompressed=false.
fn decompress_body(body: Vec<u8>, encoding: Option<&str>) -> (Vec<u8>, bool) {
    let Some(enc) = encoding else {
        return (body, false);
    };

    match enc {
        "gzip" | "x-gzip" => {
            use std::io::Read;
            let mut decoder = flate2::read::GzDecoder::new(&body[..]);
            let mut decoded = Vec::new();
            match decoder.read_to_end(&mut decoded) {
                Ok(_) => (decoded, true),
                Err(_) => (body, false),
            }
        }
        "deflate" => {
            use std::io::Read;
            let mut decoder = flate2::read::DeflateDecoder::new(&body[..]);
            let mut decoded = Vec::new();
            match decoder.read_to_end(&mut decoded) {
                Ok(_) => (decoded, true),
                Err(_) => (body, false),
            }
        }
        "br" => {
            use std::io::Read;
            let mut decoded = Vec::new();
            let mut decoder = brotli::Decompressor::new(&body[..], 4096);
            match decoder.read_to_end(&mut decoded) {
                Ok(_) => (decoded, true),
                Err(_) => (body, false),
            }
        }
        // zstd and unknown: keep compressed, don't strip headers
        _ => (body, false),
    }
}

/// Wrap a response body to count bytes flowing through it.
/// Updates the request log entry with the actual size when the body completes.
fn with_size_counting(
    state: Arc<ProxyState>,
    log_id: u64,
    res: Response<Body>,
) -> Response<Body> {
    let (parts, body) = res.into_parts();
    let (tx, rx) = mpsc::channel::<Result<Frame<bytes::Bytes>, hudsucker::Error>>(32);

    tokio::spawn(async move {
        let mut total = 0i64;
        let mut body = body;
        loop {
            match body.frame().await {
                Some(Ok(frame)) => {
                    if let Some(data) = frame.data_ref() {
                        total += data.len() as i64;
                    }
                    if tx.send(Ok(frame)).await.is_err() {
                        break;
                    }
                }
                Some(Err(e)) => {
                    let _ = tx.send(Err(e)).await;
                    break;
                }
                None => break,
            }
        }
        if total > 0 {
            state.update_request_size(log_id, total);
        }
    });

    let stream = ReceiverStream::new(rx);
    let stream_body = StreamBody::new(stream);
    Response::from_parts(parts, Body::from(stream_body))
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}
