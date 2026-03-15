use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Json};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::sync::atomic::Ordering;

use crate::cache::store;
use crate::proxy::handler::ProxyState;

#[derive(Serialize)]
pub struct HealthResponse {
    pub status: &'static str,
}

pub async fn health() -> Json<HealthResponse> {
    Json(HealthResponse { status: "ok" })
}

#[derive(Serialize)]
pub struct StatsResponse {
    pub requests: u64,
    pub cache_hits: u64,
    pub cache_misses: u64,
    /// Cumulative bandwidth not re-fetched from origin (grows with every hit)
    pub bandwidth_saved: u64,
    pub bandwidth_saved_human: String,
    pub hit_rate: f64,
    pub bypass_enabled: bool,
    pub system_proxy_enabled: bool,
    pub active_entries: i64,
    pub stale_entries: i64,
    /// Actual disk usage of active cache entries
    pub active_size: i64,
    pub active_size_human: String,
    pub total_size: i64,
    pub total_size_human: String,
    pub max_cache_size: u64,
    pub max_cache_size_human: String,
    pub image_count: i64,
    pub video_count: i64,
    pub audio_count: i64,
}

pub async fn stats(State(state): State<Arc<ProxyState>>) -> Json<StatsResponse> {
    let proxy_stats = state.stats();
    let cache_stats = state.cache_index.stats().await.unwrap_or_else(|_| {
        crate::cache::index::CacheStats {
            active_entries: 0,
            stale_entries: 0,
            active_size: 0,
            total_size: 0,
            image_count: 0,
            video_count: 0,
            audio_count: 0,
        }
    });

    let hit_rate = if proxy_stats.requests > 0 {
        proxy_stats.cache_hits as f64 / proxy_stats.requests as f64 * 100.0
    } else {
        0.0
    };

    Json(StatsResponse {
        requests: proxy_stats.requests,
        cache_hits: proxy_stats.cache_hits,
        cache_misses: proxy_stats.cache_misses,
        bandwidth_saved: proxy_stats.bytes_saved,
        bandwidth_saved_human: format_bytes(proxy_stats.bytes_saved),
        hit_rate,
        bypass_enabled: proxy_stats.bypass_enabled,
        system_proxy_enabled: proxy_stats.system_proxy_enabled,
        active_entries: cache_stats.active_entries,
        stale_entries: cache_stats.stale_entries,
        active_size: cache_stats.active_size,
        active_size_human: format_bytes(cache_stats.active_size as u64),
        total_size: cache_stats.total_size,
        total_size_human: format_bytes(cache_stats.total_size as u64),
        max_cache_size: state.max_cache_size.load(Ordering::Relaxed),
        max_cache_size_human: format_bytes(state.max_cache_size.load(Ordering::Relaxed)),
        image_count: cache_stats.image_count,
        video_count: cache_stats.video_count,
        audio_count: cache_stats.audio_count,
    })
}

#[derive(Deserialize)]
pub struct ListParams {
    pub host: Option<String>,
    pub media_type: Option<String>,
    pub status: Option<String>,
    pub q: Option<String>,
    pub offset: Option<i64>,
    pub limit: Option<i64>,
}

#[derive(Serialize)]
pub struct ListResponse {
    pub entries: Vec<EntryResponse>,
    pub total: i64,
    pub offset: i64,
    pub limit: i64,
}

#[derive(Serialize)]
pub struct EntryResponse {
    pub fingerprint: String,
    pub url: String,
    pub method: String,
    pub status_code: u16,
    pub content_type: Option<String>,
    pub file_path: String,
    pub file_size: i64,
    pub host: String,
    pub media_type: Option<String>,
    pub status: String,
    pub created_at: i64,
    pub last_accessed: i64,
    pub stale_at: Option<i64>,
}

impl From<crate::cache::index::CacheEntry> for EntryResponse {
    fn from(e: crate::cache::index::CacheEntry) -> Self {
        Self {
            fingerprint: e.fingerprint,
            url: e.url,
            method: e.method,
            status_code: e.status_code,
            content_type: e.content_type,
            file_path: e.file_path,
            file_size: e.file_size,
            host: e.host,
            media_type: e.media_type,
            status: e.status,
            created_at: e.created_at,
            last_accessed: e.last_accessed,
            stale_at: e.stale_at,
        }
    }
}

pub async fn list_entries(
    State(state): State<Arc<ProxyState>>,
    Query(params): Query<ListParams>,
) -> Result<Json<ListResponse>, StatusCode> {
    let offset = params.offset.unwrap_or(0);
    let limit = params.limit.unwrap_or(50).min(200);

    let (entries, total) = state
        .cache_index
        .list_entries(
            params.host,
            params.media_type,
            params.status,
            params.q,
            offset,
            limit,
        )
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok(Json(ListResponse {
        entries: entries.into_iter().map(EntryResponse::from).collect(),
        total,
        offset,
        limit,
    }))
}

pub async fn get_entry(
    State(state): State<Arc<ProxyState>>,
    Path(fingerprint): Path<String>,
) -> Result<Json<EntryResponse>, StatusCode> {
    // Look up in both active and stale
    let entry = state
        .cache_index
        .list_entries(None, None, None, None, 0, 1)
        .await
        .ok()
        .and_then(|(entries, _)| entries.into_iter().find(|e| e.fingerprint == fingerprint));

    // Fallback: direct lookup ignoring status
    if let Some(entry) = entry {
        return Ok(Json(EntryResponse::from(entry)));
    }

    Err(StatusCode::NOT_FOUND)
}

#[derive(Deserialize)]
pub struct DeleteParams {
    pub permanent: Option<bool>,
}

pub async fn delete_entry(
    State(state): State<Arc<ProxyState>>,
    Path(fingerprint): Path<String>,
    Query(params): Query<DeleteParams>,
) -> StatusCode {
    if params.permanent.unwrap_or(false) {
        if let Ok(Some(file_path)) = state.cache_index.delete(&fingerprint).await {
            let _ = store::delete_file(&state.cache_dir, &std::path::PathBuf::from(&file_path));
        }
    } else {
        // Mark stale
        if let Ok(Some(entry)) = state.cache_index.lookup(&fingerprint).await {
            let old_path = std::path::PathBuf::from(&entry.file_path);
            if let Ok(stale_path) = store::rename_to_stale(&state.cache_dir, &old_path) {
                let _ = state
                    .cache_index
                    .mark_stale(&fingerprint, &stale_path.to_string_lossy())
                    .await;
            }
        }
    }
    StatusCode::NO_CONTENT
}

pub async fn restore_entry(
    State(state): State<Arc<ProxyState>>,
    Path(fingerprint): Path<String>,
) -> StatusCode {
    match state.cache_index.restore(&fingerprint).await {
        Ok(()) => StatusCode::NO_CONTENT,
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

pub async fn clear_cache(
    State(state): State<Arc<ProxyState>>,
    Query(params): Query<DeleteParams>,
) -> Json<serde_json::Value> {
    if params.permanent.unwrap_or(false) {
        let count = state.cache_index.delete_all().await.unwrap_or(0);
        let cache_dir = &state.cache_dir;
        if cache_dir.exists() {
            let _ = std::fs::remove_dir_all(cache_dir);
            let _ = std::fs::create_dir_all(cache_dir);
        }
        Json(serde_json::json!({ "deleted": count }))
    } else {
        let count = state.cache_index.mark_all_stale().await.unwrap_or(0);
        Json(serde_json::json!({ "marked_stale": count }))
    }
}

pub async fn toggle_bypass(State(state): State<Arc<ProxyState>>) -> Json<serde_json::Value> {
    let prev = state.bypass.fetch_xor(true, Ordering::Relaxed);
    let new_val = !prev;
    Json(serde_json::json!({ "bypass_enabled": new_val }))
}

#[derive(Deserialize)]
pub struct SystemProxyRequest {
    pub enabled: bool,
}

pub async fn set_system_proxy(
    State(state): State<Arc<ProxyState>>,
    Json(req): Json<SystemProxyRequest>,
) -> Json<serde_json::Value> {
    use crate::macos::system_proxy;

    if req.enabled {
        match system_proxy::set_proxy_on_all_services(state.proxy_port) {
            Ok(()) => {
                state.system_proxy_enabled.store(true, Ordering::Relaxed);
                tracing::info!("System proxy enabled");
            }
            Err(e) => {
                tracing::error!("Failed to enable system proxy: {}", e);
                return Json(serde_json::json!({
                    "system_proxy_enabled": false,
                    "error": e.to_string()
                }));
            }
        }
    } else {
        match system_proxy::disable_proxy_on_all_services() {
            Ok(()) => {
                state.system_proxy_enabled.store(false, Ordering::Relaxed);
                tracing::info!("System proxy disabled");
            }
            Err(e) => {
                tracing::error!("Failed to disable system proxy: {}", e);
                return Json(serde_json::json!({
                    "system_proxy_enabled": true,
                    "error": e.to_string()
                }));
            }
        }
    }

    Json(serde_json::json!({
        "system_proxy_enabled": state.system_proxy_enabled.load(Ordering::Relaxed)
    }))
}

pub async fn get_config(State(state): State<Arc<ProxyState>>) -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "cache_dir": state.cache_dir.to_string_lossy(),
        "bypass_enabled": state.bypass.load(Ordering::Relaxed),
        "max_cache_size": state.max_cache_size.load(Ordering::Relaxed),
        "max_entry_size": state.max_entry_size.load(Ordering::Relaxed),
        "max_cache_size_human": format_bytes(state.max_cache_size.load(Ordering::Relaxed)),
        "max_entry_size_human": format_bytes(state.max_entry_size.load(Ordering::Relaxed)),
    }))
}

#[derive(Deserialize)]
pub struct UpdateConfigRequest {
    pub max_cache_size: Option<u64>,
    pub max_entry_size: Option<u64>,
}

pub async fn update_config(
    State(state): State<Arc<ProxyState>>,
    Json(req): Json<UpdateConfigRequest>,
) -> Json<serde_json::Value> {
    if let Some(size) = req.max_cache_size {
        state.max_cache_size.store(size, Ordering::Relaxed);
        let _ = state.cache_index.set_setting("max_cache_size", &size.to_string()).await;
        tracing::info!(max_cache_size = size, "Updated max_cache_size");
    }
    if let Some(size) = req.max_entry_size {
        state.max_entry_size.store(size, Ordering::Relaxed);
        let _ = state.cache_index.set_setting("max_entry_size", &size.to_string()).await;
        tracing::info!(max_entry_size = size, "Updated max_entry_size");
    }

    Json(serde_json::json!({
        "max_cache_size": state.max_cache_size.load(Ordering::Relaxed),
        "max_entry_size": state.max_entry_size.load(Ordering::Relaxed),
        "max_cache_size_human": format_bytes(state.max_cache_size.load(Ordering::Relaxed)),
        "max_entry_size_human": format_bytes(state.max_entry_size.load(Ordering::Relaxed)),
    }))
}

fn format_bytes(bytes: u64) -> String {
    if bytes >= 1_073_741_824 {
        format!("{:.1} GB", bytes as f64 / 1_073_741_824.0)
    } else if bytes >= 1_048_576 {
        format!("{:.1} MB", bytes as f64 / 1_048_576.0)
    } else if bytes >= 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{} B", bytes)
    }
}

pub async fn serve_cache_file(
    State(state): State<Arc<ProxyState>>,
    Path(path): Path<String>,
) -> impl IntoResponse {
    let file_path = state.cache_dir.join(&path);

    match std::fs::read(&file_path) {
        Ok(body) => {
            let mime = mime_guess::from_path(&path)
                .first_or_octet_stream()
                .to_string();

            (
                StatusCode::OK,
                [
                    ("content-type", mime),
                    ("cache-control", "public, max-age=3600".to_string()),
                ],
                body,
            )
                .into_response()
        }
        Err(_) => StatusCode::NOT_FOUND.into_response(),
    }
}

#[derive(Serialize)]
pub struct MediaGroup {
    pub host: String,
    pub entries: Vec<MediaEntry>,
    pub total_size: i64,
}

#[derive(Serialize)]
pub struct MediaEntry {
    pub fingerprint: String,
    pub url: String,
    pub content_type: Option<String>,
    pub media_type: Option<String>,
    pub file_path: String,
    pub file_size: i64,
    pub thumbnail_url: String,
}

#[derive(Deserialize)]
pub struct RequestLogParams {
    pub since: Option<u64>,
}

pub async fn list_requests(
    State(state): State<Arc<ProxyState>>,
    Query(params): Query<RequestLogParams>,
) -> Json<Vec<crate::proxy::handler::RequestLogEntry>> {
    let since = params.since.unwrap_or(0);
    Json(state.get_requests_since(since))
}

pub async fn list_media(
    State(state): State<Arc<ProxyState>>,
    Query(params): Query<ListParams>,
) -> Result<Json<Vec<MediaGroup>>, StatusCode> {
    let media_filter = params.media_type.or(Some("image".to_string()));

    let (entries, _) = state
        .cache_index
        .list_entries(
            params.host,
            media_filter,
            Some("active".to_string()),
            params.q,
            0,
            500,
        )
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    // Group by host
    let mut groups: std::collections::HashMap<String, Vec<MediaEntry>> =
        std::collections::HashMap::new();

    for entry in entries {
        let thumbnail_url = format!("/api/cache/file/{}", entry.file_path);
        groups
            .entry(entry.host.clone())
            .or_default()
            .push(MediaEntry {
                fingerprint: entry.fingerprint,
                url: entry.url,
                content_type: entry.content_type,
                media_type: entry.media_type,
                file_path: entry.file_path,
                file_size: entry.file_size,
                thumbnail_url,
            });
    }

    let result: Vec<MediaGroup> = groups
        .into_iter()
        .map(|(host, entries)| {
            let total_size = entries.iter().map(|e| e.file_size).sum();
            MediaGroup {
                host,
                entries,
                total_size,
            }
        })
        .collect();

    Ok(Json(result))
}
