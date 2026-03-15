use serde::{Deserialize, Serialize};
use std::path::Path;
use std::process::Command;
use tracing::{info, warn};

use crate::error::Error;

/// Saved proxy state for one network service, used for restoration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServiceProxyState {
    pub service: String,
    pub web_proxy_enabled: bool,
    pub web_proxy_server: String,
    pub web_proxy_port: String,
    pub secure_proxy_enabled: bool,
    pub secure_proxy_server: String,
    pub secure_proxy_port: String,
}

/// All saved proxy state, persisted to proxy-state.json.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SavedProxyState {
    pub services: Vec<ServiceProxyState>,
}

/// Detect all active network services.
pub fn detect_active_services() -> Result<Vec<String>, Error> {
    // Get the primary interface from scutil --nwi
    let nwi_output = Command::new("scutil")
        .args(["--nwi"])
        .output()
        .map_err(|e| Error::Proxy(format!("Failed to run scutil --nwi: {}", e)))?;

    let nwi_text = String::from_utf8_lossy(&nwi_output.stdout);

    // Extract interface names (e.g., en0, en1) from "Network interfaces:" lines
    let mut interfaces: Vec<String> = Vec::new();
    let mut in_ipv4 = false;
    for line in nwi_text.lines() {
        if line.contains("IPv4 network interface information") {
            in_ipv4 = true;
            continue;
        }
        if line.contains("IPv6 network interface information") {
            break;
        }
        if in_ipv4 {
            let trimmed = line.trim();
            // Lines like "en0 : flags ..." indicate an active interface
            if let Some(iface) = trimmed.split_whitespace().next() {
                if iface.starts_with("en") || iface.starts_with("utun") || iface.starts_with("bridge") {
                    if !interfaces.contains(&iface.to_string()) {
                        interfaces.push(iface.to_string());
                    }
                }
            }
        }
    }

    // Map interface names to network service names
    let hw_output = Command::new("networksetup")
        .args(["-listallhardwareports"])
        .output()
        .map_err(|e| Error::Proxy(format!("Failed to run networksetup: {}", e)))?;

    let hw_text = String::from_utf8_lossy(&hw_output.stdout);
    let mut services: Vec<String> = Vec::new();
    let mut current_service: Option<String> = None;

    for line in hw_text.lines() {
        if let Some(name) = line.strip_prefix("Hardware Port: ") {
            current_service = Some(name.to_string());
        } else if let Some(device) = line.strip_prefix("Device: ") {
            if let Some(ref svc) = current_service {
                if interfaces.contains(&device.to_string()) {
                    services.push(svc.clone());
                }
            }
        }
    }

    // Fallback: if we found no services, try "Wi-Fi" and "Ethernet"
    if services.is_empty() {
        for fallback in &["Wi-Fi", "Ethernet", "USB 10/100/1000 LAN"] {
            let check = Command::new("networksetup")
                .args(["-getwebproxy", fallback])
                .output();
            if let Ok(output) = check {
                if output.status.success() {
                    services.push(fallback.to_string());
                }
            }
        }
    }

    info!("Detected active network services: {:?}", services);
    Ok(services)
}

/// Read current proxy settings for a network service.
fn read_proxy_state(service: &str) -> Result<ServiceProxyState, Error> {
    let web = Command::new("networksetup")
        .args(["-getwebproxy", service])
        .output()
        .map_err(|e| Error::Proxy(format!("Failed to read web proxy for {}: {}", service, e)))?;

    let web_text = String::from_utf8_lossy(&web.stdout);
    let web_enabled = web_text.lines().any(|l| l.starts_with("Enabled: Yes"));
    let web_server = parse_field(&web_text, "Server: ");
    let web_port = parse_field(&web_text, "Port: ");

    let secure = Command::new("networksetup")
        .args(["-getsecurewebproxy", service])
        .output()
        .map_err(|e| Error::Proxy(format!("Failed to read secure proxy for {}: {}", service, e)))?;

    let secure_text = String::from_utf8_lossy(&secure.stdout);
    let secure_enabled = secure_text.lines().any(|l| l.starts_with("Enabled: Yes"));
    let secure_server = parse_field(&secure_text, "Server: ");
    let secure_port = parse_field(&secure_text, "Port: ");

    Ok(ServiceProxyState {
        service: service.to_string(),
        web_proxy_enabled: web_enabled,
        web_proxy_server: web_server,
        web_proxy_port: web_port,
        secure_proxy_enabled: secure_enabled,
        secure_proxy_server: secure_server,
        secure_proxy_port: secure_port,
    })
}

fn parse_field(text: &str, prefix: &str) -> String {
    text.lines()
        .find_map(|l| l.strip_prefix(prefix))
        .unwrap_or("")
        .trim()
        .to_string()
}

/// Save current proxy state for all active services, then set proxy.
pub fn enable_system_proxy(state_path: &Path, port: u16) -> Result<(), Error> {
    // Check for orphaned state first
    if state_path.exists() {
        warn!("Found orphaned proxy-state.json — restoring previous proxy state first");
        if let Err(e) = restore_system_proxy(state_path) {
            warn!("Failed to restore orphaned state: {}", e);
        }
    }

    let services = detect_active_services()?;
    if services.is_empty() {
        warn!("No active network services found — skipping system proxy configuration");
        return Ok(());
    }

    // Save current state
    let mut states = Vec::new();
    for svc in &services {
        match read_proxy_state(svc) {
            Ok(state) => states.push(state),
            Err(e) => warn!("Failed to read proxy state for {}: {}", svc, e),
        }
    }

    let saved = SavedProxyState { services: states };
    let json = serde_json::to_string_pretty(&saved)
        .map_err(|e| Error::Proxy(format!("Failed to serialize proxy state: {}", e)))?;
    std::fs::write(state_path, json)?;
    info!("Saved proxy state to {}", state_path.display());

    // Set proxy on all services
    let port_str = port.to_string();
    for svc in &services {
        run_networksetup(&["-setwebproxy", svc, "127.0.0.1", &port_str])?;
        run_networksetup(&["-setsecurewebproxy", svc, "127.0.0.1", &port_str])?;
        info!("Set HTTP+HTTPS proxy on '{}' -> 127.0.0.1:{}", svc, port);
    }

    Ok(())
}

/// Restore proxy state from saved file and delete the state file.
pub fn restore_system_proxy(state_path: &Path) -> Result<(), Error> {
    if !state_path.exists() {
        return Ok(());
    }

    let json = std::fs::read_to_string(state_path)?;
    let saved: SavedProxyState = serde_json::from_str(&json)
        .map_err(|e| Error::Proxy(format!("Failed to parse proxy state: {}", e)))?;

    for svc in &saved.services {
        if svc.web_proxy_enabled {
            let _ = run_networksetup(&[
                "-setwebproxy",
                &svc.service,
                &svc.web_proxy_server,
                &svc.web_proxy_port,
            ]);
        } else {
            let _ = run_networksetup(&["-setwebproxystate", &svc.service, "off"]);
        }

        if svc.secure_proxy_enabled {
            let _ = run_networksetup(&[
                "-setsecurewebproxy",
                &svc.service,
                &svc.secure_proxy_server,
                &svc.secure_proxy_port,
            ]);
        } else {
            let _ = run_networksetup(&["-setsecurewebproxystate", &svc.service, "off"]);
        }

        info!("Restored proxy settings for '{}'", svc.service);
    }

    std::fs::remove_file(state_path)?;
    info!("Removed proxy state file {}", state_path.display());
    Ok(())
}

/// Set proxy on all active network services (simple, no state file).
pub fn set_proxy_on_all_services(port: u16) -> Result<(), Error> {
    let services = detect_active_services()?;
    let port_str = port.to_string();
    for svc in &services {
        run_networksetup(&["-setwebproxy", svc, "127.0.0.1", &port_str])?;
        run_networksetup(&["-setsecurewebproxy", svc, "127.0.0.1", &port_str])?;
        // Bypass proxy for localhost to prevent the proxy from looping on itself
        run_networksetup(&["-setproxybypassdomains", svc, "localhost", "127.0.0.1", "*.local"])?;
        info!("Set HTTP+HTTPS proxy on '{}' -> 127.0.0.1:{}", svc, port);
    }
    Ok(())
}

/// Disable proxy on all active network services.
pub fn disable_proxy_on_all_services() -> Result<(), Error> {
    let services = detect_active_services()?;
    for svc in &services {
        run_networksetup(&["-setwebproxystate", svc, "off"])?;
        run_networksetup(&["-setsecurewebproxystate", svc, "off"])?;
        info!("Disabled HTTP+HTTPS proxy on '{}'", svc);
    }
    Ok(())
}

/// Synchronous restore for use in panic hooks.
pub fn restore_system_proxy_sync(state_path: &Path) {
    if let Err(e) = restore_system_proxy(state_path) {
        eprintln!("PANIC HOOK: Failed to restore system proxy: {}", e);
    }
}

fn run_networksetup(args: &[&str]) -> Result<(), Error> {
    let output = Command::new("networksetup")
        .args(args)
        .output()
        .map_err(|e| Error::Proxy(format!("Failed to run networksetup {:?}: {}", args, e)))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(Error::Proxy(format!(
            "networksetup {:?} failed: {}",
            args, stderr
        )));
    }

    Ok(())
}
