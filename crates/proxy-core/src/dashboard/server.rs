use axum::routing::{delete, get, post};
use axum::Router;
use std::net::SocketAddr;
use std::sync::Arc;
use tracing::info;

use crate::dashboard::api;
use crate::proxy::handler::ProxyState;

/// Start the dashboard HTTP server on the given port.
pub async fn start(state: Arc<ProxyState>, port: u16) {
    let app = Router::new()
        .route("/api/health", get(api::health))
        .route("/api/stats", get(api::stats))
        .route("/api/entries", get(api::list_entries))
        .route("/api/entries/{fingerprint}", get(api::get_entry))
        .route("/api/entries/{fingerprint}", delete(api::delete_entry))
        .route(
            "/api/entries/{fingerprint}/restore",
            post(api::restore_entry),
        )
        .route("/api/cache/clear", post(api::clear_cache))
        .route("/api/bypass", post(api::toggle_bypass))
        .route("/api/system-proxy", post(api::set_system_proxy))
        .route("/api/config", get(api::get_config).post(api::update_config))
        .route("/api/cache/file/{*path}", get(api::serve_cache_file))
        .route("/api/requests", get(api::list_requests))
        .route("/api/media", get(api::list_media))
        .layer(
            tower_http::cors::CorsLayer::new()
                .allow_origin(tower_http::cors::Any)
                .allow_methods(tower_http::cors::Any)
                .allow_headers(tower_http::cors::Any),
        )
        .with_state(state);

    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    info!("Dashboard listening on http://{}", addr);

    let listener = match tokio::net::TcpListener::bind(addr).await {
        Ok(l) => l,
        Err(e) => {
            tracing::error!("Failed to bind dashboard port {}: {}", port, e);
            return;
        }
    };

    if let Err(e) = axum::serve(listener, app).await {
        tracing::error!("Dashboard server error: {}", e);
    }
}
