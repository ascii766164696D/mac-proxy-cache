use hudsucker::certificate_authority::RcgenAuthority;
use hudsucker::rcgen::{Issuer, KeyPair};
use hudsucker::rustls::crypto::aws_lc_rs;
use hudsucker::Proxy;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU64};
use std::sync::Arc;
use tokio::signal;
use tracing::info;

use crate::cache::index::CacheIndex;
use crate::config::Config;
use crate::error::Error;
use crate::proxy::handler::{CachingHandler, ProxyState};
use crate::proxy::tls;

/// Start the proxy server. Blocks until SIGINT/SIGTERM.
pub async fn run(config: &Config) -> Result<(), Error> {
    // Ensure data directory exists
    std::fs::create_dir_all(&config.data_dir)?;
    std::fs::create_dir_all(config.cache_dir())?;

    // Load or generate CA certificate
    let (cert_pem, key_pem) = tls::load_or_generate_ca(&config.ca_dir())?;

    // Build the certificate authority
    let key_pair = KeyPair::from_pem(&key_pem)?;
    let issuer = Issuer::from_ca_cert_pem(&cert_pem, key_pair)?;
    let ca = RcgenAuthority::new(issuer, 1_000, aws_lc_rs::default_provider());

    // Open cache database
    let cache_index = CacheIndex::open(&config.db_path()).await?;

    // Load persisted settings from SQLite (overrides config file defaults)
    let max_cache_size = if let Ok(Some(val)) = cache_index.get_setting("max_cache_size").await {
        val.parse::<u64>().unwrap_or(config.max_cache_size)
    } else {
        config.max_cache_size
    };
    let max_entry_size = if let Ok(Some(val)) = cache_index.get_setting("max_entry_size").await {
        val.parse::<u64>().unwrap_or(config.max_entry_size)
    } else {
        config.max_entry_size
    };
    info!("Cache limits: max_cache_size={}, max_entry_size={}", max_cache_size, max_entry_size);

    // Create range cache
    let range_cache = crate::cache::range::RangeCache::new(
        cache_index.conn().clone(),
        config.cache_dir(),
    );

    // LRU touch batching channel
    let (touch_tx, mut touch_rx) = tokio::sync::mpsc::unbounded_channel();

    // Create shared state
    let state = Arc::new(ProxyState {
        cache_index: cache_index.clone(),
        range_cache,
        cache_dir: config.cache_dir(),
        bypass: AtomicBool::new(false),
        max_cache_size: AtomicU64::new(max_cache_size),
        max_entry_size: AtomicU64::new(max_entry_size),
        serve_stale_on_error: config.serve_stale_on_error,
        system_proxy_enabled: AtomicBool::new(config.auto_system_proxy),
        proxy_port: config.proxy_port,
        requests: Default::default(),
        cache_hits: Default::default(),
        cache_misses: Default::default(),
        bytes_saved: Default::default(),
        touch_tx,
        request_log: std::sync::Mutex::new(std::collections::VecDeque::new()),
        request_log_counter: Default::default(),
    });

    // Spawn LRU touch batcher
    let batch_index = cache_index.clone();
    tokio::spawn(async move {
        let mut batch = Vec::new();
        loop {
            // Drain available touches
            match touch_rx.recv().await {
                Some(item) => batch.push(item),
                None => break,
            }
            // Drain any more that are immediately available
            while let Ok(item) = touch_rx.try_recv() {
                batch.push(item);
            }
            if !batch.is_empty() {
                let updates: Vec<_> = batch.drain(..).collect();
                if let Err(e) = batch_index.touch_batch(updates).await {
                    tracing::warn!("Failed to batch update LRU timestamps: {}", e);
                }
            }
        }
    });

    // Spawn eviction background task
    crate::cache::eviction::spawn_eviction_task(cache_index.clone(), config.clone(), state.clone());

    // Spawn dashboard server
    let dashboard_state = state.clone();
    let dashboard_port = config.dashboard_port;
    tokio::spawn(async move {
        crate::dashboard::server::start(dashboard_state, dashboard_port).await;
    });

    let handler = CachingHandler::new(state.clone());

    let addr = SocketAddr::from(([127, 0, 0, 1], config.proxy_port));

    let proxy = Proxy::builder()
        .with_addr(addr)
        .with_ca(ca)
        .with_rustls_connector(aws_lc_rs::default_provider())
        .with_http_handler(handler)
        .with_graceful_shutdown(shutdown_signal())
        .build()
        .map_err(|e| Error::Proxy(e.to_string()))?;

    info!("Proxy listening on {}", addr);

    proxy
        .start()
        .await
        .map_err(|e| Error::Proxy(e.to_string()))?;

    // Print final stats
    let stats = state.stats();
    info!(
        "Final stats: {} requests, {} hits, {} misses, {} bytes saved",
        stats.requests, stats.cache_hits, stats.cache_misses, stats.bytes_saved
    );

    info!("Proxy shut down");
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        signal::ctrl_c()
            .await
            .expect("failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        signal::unix::signal(signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => info!("Received SIGINT, shutting down..."),
        _ = terminate => info!("Received SIGTERM, shutting down..."),
    }
}
