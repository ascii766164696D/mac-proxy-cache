use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub proxy_port: u16,
    pub dashboard_port: u16,
    pub data_dir: PathBuf,
    pub max_cache_size: u64,
    pub max_entry_size: u64,
    pub stale_retention_days: u32,
    pub partial_range_ttl_days: u32,
    pub bypass_hosts: Vec<String>,
    pub auto_system_proxy: bool,
    pub serve_stale_on_error: bool,
}

impl Default for Config {
    fn default() -> Self {
        let data_dir = dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("mac-proxy-cache");

        Self {
            proxy_port: 9090,
            dashboard_port: 9091,
            data_dir,
            max_cache_size: 1_073_741_824,  // 1 GB
            max_entry_size: 524_288_000,     // 500 MB
            stale_retention_days: 30,
            partial_range_ttl_days: 7,
            bypass_hosts: Vec::new(),
            auto_system_proxy: true,
            serve_stale_on_error: false,
        }
    }
}

impl Config {
    /// Load config from the default config file path, falling back to defaults.
    pub fn load() -> Self {
        let config_path = Self::config_file_path();
        if config_path.exists() {
            match std::fs::read_to_string(&config_path) {
                Ok(contents) => match toml::from_str(&contents) {
                    Ok(config) => return config,
                    Err(e) => {
                        tracing::warn!("Failed to parse config file {}: {}", config_path.display(), e);
                    }
                },
                Err(e) => {
                    tracing::warn!("Failed to read config file {}: {}", config_path.display(), e);
                }
            }
        }
        Self::default()
    }

    /// Path to the config file: ~/.config/mac-proxy-cache/config.toml
    pub fn config_file_path() -> PathBuf {
        dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from(".config"))
            .join("mac-proxy-cache")
            .join("config.toml")
    }

    pub fn ca_dir(&self) -> PathBuf {
        self.data_dir.join("ca")
    }

    pub fn cache_dir(&self) -> PathBuf {
        self.data_dir.join("cache")
    }

    pub fn db_path(&self) -> PathBuf {
        self.data_dir.join("index.db")
    }

    pub fn pid_path(&self) -> PathBuf {
        self.data_dir.join("proxy.pid")
    }

    pub fn proxy_state_path(&self) -> PathBuf {
        self.data_dir.join("proxy-state.json")
    }
}
