use std::path::Path;
use std::process::Command;

use crate::error::Error;

/// Print the CA certificate path and SHA-256 fingerprint.
pub fn show_cert(ca_dir: &Path) -> Result<(), Error> {
    let cert_path = ca_dir.join("ca.crt");
    if !cert_path.exists() {
        return Err(Error::Config(
            "CA certificate not found. Run the proxy first to generate it.".into(),
        ));
    }

    println!("CA Certificate: {}", cert_path.display());

    // Get SHA-256 fingerprint using openssl
    let output = Command::new("openssl")
        .args([
            "x509",
            "-in",
            &cert_path.to_string_lossy(),
            "-noout",
            "-fingerprint",
            "-sha256",
        ])
        .output()
        .map_err(|e| Error::Proxy(format!("Failed to run openssl: {}", e)))?;

    if output.status.success() {
        let fingerprint = String::from_utf8_lossy(&output.stdout);
        println!("SHA-256 Fingerprint: {}", fingerprint.trim());
    } else {
        println!("(Could not compute fingerprint — openssl not available)");
    }

    Ok(())
}

/// Install the CA certificate into the system trust store.
/// This requires admin privileges and will prompt for a password.
pub fn install_cert(ca_dir: &Path) -> Result<(), Error> {
    let cert_path = ca_dir.join("ca.crt");
    if !cert_path.exists() {
        return Err(Error::Config(
            "CA certificate not found. Run the proxy first to generate it.".into(),
        ));
    }

    println!("Installing CA certificate into system trust store...");
    println!("You may be prompted for your password.");
    println!();

    let status = Command::new("security")
        .args([
            "add-trusted-cert",
            "-d",
            "-r",
            "trustRoot",
            "-k",
            "/Library/Keychains/System.keychain",
            &cert_path.to_string_lossy(),
        ])
        .status()
        .map_err(|e| Error::Proxy(format!("Failed to run security command: {}", e)))?;

    if status.success() {
        println!("CA certificate installed successfully.");
        println!("Browsers will now trust HTTPS connections through the proxy.");
    } else {
        return Err(Error::Proxy(
            "Failed to install CA certificate. You may need to run with sudo.".into(),
        ));
    }

    Ok(())
}

/// Export (copy) the CA certificate to a user-specified path.
pub fn export_cert(ca_dir: &Path, dest: &Path) -> Result<(), Error> {
    let cert_path = ca_dir.join("ca.crt");
    if !cert_path.exists() {
        return Err(Error::Config(
            "CA certificate not found. Run the proxy first to generate it.".into(),
        ));
    }

    std::fs::copy(&cert_path, dest)?;
    println!("CA certificate exported to {}", dest.display());

    Ok(())
}
