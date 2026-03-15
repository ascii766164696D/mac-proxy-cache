use anyhow::Result;
use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;

use proxy_core::config::Config;

#[derive(Parser)]
#[command(name = "mac-proxy-cache", about = "macOS caching proxy server")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Start the proxy server
    Start {
        /// Run in foreground (don't daemonize)
        #[arg(long)]
        foreground: bool,

        /// Proxy listen port
        #[arg(long)]
        port: Option<u16>,

        /// Dashboard listen port
        #[arg(long)]
        dashboard_port: Option<u16>,

        /// Don't set the macOS system proxy
        #[arg(long)]
        no_system_proxy: bool,

        /// Maximum cache size (e.g. "1G", "500M")
        #[arg(long)]
        max_cache_size: Option<String>,

        /// Start with cache bypass enabled
        #[arg(long)]
        bypass_cache: bool,
    },

    /// Stop the running proxy server
    Stop,

    /// Show proxy status
    Status,

    /// Cache management commands
    Cache {
        #[command(subcommand)]
        action: CacheAction,
    },

    /// Manage the CA certificate
    Cert {
        #[command(subcommand)]
        action: CertAction,
    },
}

#[derive(Subcommand)]
enum CacheAction {
    /// Show cache statistics
    Stats,
    /// Clear all cached entries
    Clear {
        /// Permanently delete files instead of marking stale
        #[arg(long)]
        permanent: bool,
    },
    /// Search cached entries by URL
    Search {
        /// Search query (matched against URLs)
        query: String,
    },
}

#[derive(Subcommand)]
enum CertAction {
    /// Show CA certificate path and fingerprint
    Show,
    /// Install CA certificate into system trust store
    Install,
    /// Export CA certificate to a file
    Export {
        /// Destination file path
        path: String,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Start {
            foreground,
            port,
            dashboard_port,
            no_system_proxy,
            max_cache_size,
            bypass_cache,
        } => {
            if !foreground {
                anyhow::bail!("Background mode not yet implemented. Use --foreground.");
            }

            tracing_subscriber::fmt()
                .with_env_filter(
                    EnvFilter::try_from_default_env()
                        .unwrap_or_else(|_| EnvFilter::new("info")),
                )
                .init();

            let mut config = Config::load();
            if let Some(p) = port {
                config.proxy_port = p;
            }
            if let Some(p) = dashboard_port {
                config.dashboard_port = p;
            }
            if no_system_proxy {
                config.auto_system_proxy = false;
            }
            if let Some(size) = max_cache_size {
                config.max_cache_size = parse_size(&size)?;
            }

            tracing::info!("Starting proxy with config: {:?}", config);

            // Ensure data dir exists
            std::fs::create_dir_all(&config.data_dir)?;

            // Write PID file
            let pid_path = config.pid_path();
            if pid_path.exists() {
                // Check if the PID is still alive
                if let Ok(pid_str) = std::fs::read_to_string(&pid_path) {
                    if let Ok(pid) = pid_str.trim().parse::<i32>() {
                        if is_process_alive(pid) {
                            anyhow::bail!(
                                "Proxy is already running (PID {}). Use 'stop' first.",
                                pid
                            );
                        }
                    }
                }
                // Stale PID file — remove it
                let _ = std::fs::remove_file(&pid_path);
            }
            std::fs::write(&pid_path, std::process::id().to_string())?;

            // Crash recovery: check for orphaned proxy state
            let state_path = config.proxy_state_path();
            if state_path.exists() {
                tracing::warn!("Found orphaned proxy state — restoring before starting");
                if let Err(e) =
                    proxy_core::macos::system_proxy::restore_system_proxy(&state_path)
                {
                    tracing::warn!("Failed to restore orphaned state: {}", e);
                }
            }

            // Set up system proxy
            if config.auto_system_proxy {
                proxy_core::macos::system_proxy::enable_system_proxy(
                    &state_path,
                    config.proxy_port,
                )?;

                let panic_state_path = state_path.clone();
                let default_hook = std::panic::take_hook();
                std::panic::set_hook(Box::new(move |info| {
                    eprintln!("PANIC: Restoring system proxy settings...");
                    proxy_core::macos::system_proxy::restore_system_proxy_sync(
                        &panic_state_path,
                    );
                    default_hook(info);
                }));
            }

            // Set bypass mode if requested
            if bypass_cache {
                tracing::info!("Cache bypass mode enabled");
            }

            // Run the proxy (blocks until shutdown signal)
            let result = proxy_core::proxy::engine::run(&config).await;

            // Restore system proxy on clean shutdown
            if config.auto_system_proxy {
                if let Err(e) =
                    proxy_core::macos::system_proxy::restore_system_proxy(&state_path)
                {
                    tracing::error!("Failed to restore system proxy on shutdown: {}", e);
                }
            }

            // Remove PID file
            let _ = std::fs::remove_file(&pid_path);

            result?;
        }

        Commands::Stop => {
            let config = Config::load();
            let pid_path = config.pid_path();

            if !pid_path.exists() {
                println!("Proxy is not running (no PID file found).");
                return Ok(());
            }

            let pid_str = std::fs::read_to_string(&pid_path)?;
            let pid: i32 = pid_str.trim().parse().map_err(|_| {
                anyhow::anyhow!("Invalid PID file contents: {}", pid_str.trim())
            })?;

            if !is_process_alive(pid) {
                println!("Proxy process {} is not running. Cleaning up stale PID file.", pid);
                let _ = std::fs::remove_file(&pid_path);
                return Ok(());
            }

            println!("Sending SIGTERM to proxy (PID {})...", pid);
            unsafe {
                libc::kill(pid, libc::SIGTERM);
            }

            // Wait for process to exit (up to 5 seconds)
            for _ in 0..50 {
                if !is_process_alive(pid) {
                    println!("Proxy stopped.");
                    let _ = std::fs::remove_file(&pid_path);
                    return Ok(());
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }

            println!("Proxy did not stop within 5 seconds. You may need to kill -9 {}.", pid);
        }

        Commands::Status => {
            let config = Config::load();
            let pid_path = config.pid_path();

            if !pid_path.exists() {
                println!("Proxy is not running.");
                return Ok(());
            }

            let pid_str = std::fs::read_to_string(&pid_path)?;
            let pid: i32 = pid_str.trim().parse().unwrap_or(0);

            if !is_process_alive(pid) {
                println!("Proxy is not running (stale PID file for PID {}).", pid);
                return Ok(());
            }

            println!("Proxy is running (PID {}).", pid);
            println!("  Port: {}", config.proxy_port);
            println!("  Dashboard: http://127.0.0.1:{}", config.dashboard_port);
            println!("  Data dir: {}", config.data_dir.display());

            // Show cache stats if DB exists
            if config.db_path().exists() {
                let index = proxy_core::cache::index::CacheIndex::open(&config.db_path()).await?;
                let stats = index.stats().await?;
                println!();
                println!("Cache:");
                println!("  Active entries: {}", stats.active_entries);
                println!("  Stale entries: {}", stats.stale_entries);
                println!("  Active size: {}", format_size(stats.active_size));
                println!("  Total size: {}", format_size(stats.total_size));
                println!("  Images: {}", stats.image_count);
                println!("  Videos: {}", stats.video_count);
                println!("  Audio: {}", stats.audio_count);
            }
        }

        Commands::Cache { action } => {
            let config = Config::load();

            if !config.db_path().exists() {
                println!("No cache database found. Start the proxy first.");
                return Ok(());
            }

            let index = proxy_core::cache::index::CacheIndex::open(&config.db_path()).await?;

            match action {
                CacheAction::Stats => {
                    let stats = index.stats().await?;
                    println!("Cache Statistics:");
                    println!("  Active entries: {}", stats.active_entries);
                    println!("  Stale entries:  {}", stats.stale_entries);
                    println!("  Active size:    {}", format_size(stats.active_size));
                    println!("  Total size:     {}", format_size(stats.total_size));
                    println!("  Images:         {}", stats.image_count);
                    println!("  Videos:         {}", stats.video_count);
                    println!("  Audio:          {}", stats.audio_count);
                }

                CacheAction::Clear { permanent } => {
                    if permanent {
                        let count = index.delete_all().await?;
                        // Also remove cache files
                        let cache_dir = config.cache_dir();
                        if cache_dir.exists() {
                            let _ = std::fs::remove_dir_all(&cache_dir);
                            let _ = std::fs::create_dir_all(&cache_dir);
                        }
                        println!("Permanently deleted {} cache entries.", count);
                    } else {
                        let count = index.mark_all_stale().await?;
                        println!("Marked {} entries as stale.", count);
                    }
                }

                CacheAction::Search { query } => {
                    let entries = index.search(&query, 50).await?;
                    if entries.is_empty() {
                        println!("No entries found matching '{}'.", query);
                    } else {
                        println!("Found {} entries:", entries.len());
                        for entry in &entries {
                            println!(
                                "  [{}] {} ({}, {})",
                                entry.status,
                                entry.url,
                                entry.content_type.as_deref().unwrap_or("unknown"),
                                format_size(entry.file_size),
                            );
                        }
                    }
                }
            }
        }

        Commands::Cert { action } => {
            let config = Config::load();
            match action {
                CertAction::Show => {
                    proxy_core::macos::cert_install::show_cert(&config.ca_dir())?;
                }
                CertAction::Install => {
                    proxy_core::macos::cert_install::install_cert(&config.ca_dir())?;
                }
                CertAction::Export { path } => {
                    proxy_core::macos::cert_install::export_cert(
                        &config.ca_dir(),
                        std::path::Path::new(&path),
                    )?;
                }
            }
        }
    }

    Ok(())
}

fn is_process_alive(pid: i32) -> bool {
    unsafe { libc::kill(pid, 0) == 0 }
}

fn format_size(bytes: i64) -> String {
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

fn parse_size(s: &str) -> Result<u64> {
    let s = s.trim();
    let (num, multiplier) = if let Some(n) = s.strip_suffix(['G', 'g']) {
        (n.trim(), 1_073_741_824u64)
    } else if let Some(n) = s.strip_suffix("GB") {
        (n.trim(), 1_073_741_824u64)
    } else if let Some(n) = s.strip_suffix(['M', 'm']) {
        (n.trim(), 1_048_576u64)
    } else if let Some(n) = s.strip_suffix("MB") {
        (n.trim(), 1_048_576u64)
    } else if let Some(n) = s.strip_suffix(['K', 'k']) {
        (n.trim(), 1_024u64)
    } else if let Some(n) = s.strip_suffix("KB") {
        (n.trim(), 1_024u64)
    } else {
        (s, 1u64)
    };

    let value: f64 = num
        .parse()
        .map_err(|_| anyhow::anyhow!("Invalid size: {}", s))?;
    Ok((value * multiplier as f64) as u64)
}
