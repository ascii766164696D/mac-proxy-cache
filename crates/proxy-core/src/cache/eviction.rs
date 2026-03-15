use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::time::{Duration, interval};

use crate::cache::index::CacheIndex;
use crate::cache::store;
use crate::config::Config;
use crate::proxy::handler::ProxyState;

/// Spawn the background eviction task.
pub fn spawn_eviction_task(index: CacheIndex, config: Config, state: Arc<ProxyState>) {
    tokio::spawn(async move {
        let mut ticker = interval(Duration::from_secs(60));
        loop {
            ticker.tick().await;
            if let Err(e) = run_eviction_cycle(&index, &config, &state).await {
                tracing::warn!("Eviction cycle failed: {}", e);
            }
        }
    });
}

async fn run_eviction_cycle(
    index: &CacheIndex,
    config: &Config,
    state: &ProxyState,
) -> Result<(), crate::error::Error> {
    let now = now_unix();
    let cache_dir = config.cache_dir();

    // 1. TTL sweep: mark expired active entries as stale
    let expired = index.get_expired_active().await?;
    for (fp, file_path) in &expired {
        let path = PathBuf::from(file_path);
        match store::rename_to_stale(&cache_dir, &path) {
            Ok(stale_path) => {
                let _ = index.mark_stale(fp, &stale_path.to_string_lossy()).await;
            }
            Err(e) => tracing::warn!(path = %file_path, "TTL rename failed: {}", e),
        }
    }
    if !expired.is_empty() {
        tracing::info!(count = expired.len(), "TTL sweep: marked entries stale");
    }

    // 2. Stale retention: permanently delete old stale entries
    let retention_secs = config.stale_retention_days as i64 * 86400;
    let cutoff = now - retention_secs;
    let old_stale = index.get_stale_older_than(cutoff).await?;
    for (fp, file_path) in &old_stale {
        let _ = store::delete_file(&cache_dir, &PathBuf::from(file_path));
        let _ = index.delete(fp).await;
    }
    if !old_stale.is_empty() {
        tracing::info!(
            count = old_stale.len(),
            "Stale retention: permanently deleted old entries"
        );
    }

    // 3. Size-based LRU eviction (reads live max_cache_size)
    let total_size = index.total_size_all().await?;
    let max_size = state.max_cache_size.load(Ordering::Relaxed) as i64;

    if total_size > max_size {
        let target = (max_size as f64 * 0.9) as i64;
        let mut freed: i64 = 0;
        let to_free = total_size - target;

        // First evict stale entries
        let stale = index.get_stale_for_eviction(100).await?;
        for (fp, file_path, file_size) in &stale {
            if freed >= to_free {
                break;
            }
            let _ = store::delete_file(&cache_dir, &PathBuf::from(file_path));
            let _ = index.delete(fp).await;
            freed += file_size;
        }

        // If still over budget, evict active entries by LRU
        if freed < to_free {
            let active = index.get_active_for_eviction(100).await?;
            for (fp, file_path, file_size) in &active {
                if freed >= to_free {
                    break;
                }
                let _ = store::delete_file(&cache_dir, &PathBuf::from(file_path));
                let _ = index.delete(fp).await;
                freed += file_size;
            }
        }

        if freed > 0 {
            tracing::info!(
                freed_bytes = freed,
                total_was = total_size,
                max = max_size,
                "LRU eviction: freed space"
            );
        }
    }

    Ok(())
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}
