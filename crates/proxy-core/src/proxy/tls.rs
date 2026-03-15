use rcgen::{BasicConstraints, CertificateParams, DistinguishedName, DnType, IsCa, KeyPair};
use std::path::Path;
use tracing::info;

use crate::error::Error;

/// Load an existing CA or generate a new one. Returns (cert_pem, key_pem).
pub fn load_or_generate_ca(ca_dir: &Path) -> Result<(String, String), Error> {
    let cert_path = ca_dir.join("ca.crt");
    let key_path = ca_dir.join("ca.key");

    if cert_path.exists() && key_path.exists() {
        info!("Loading existing CA from {}", ca_dir.display());
        let cert_pem = std::fs::read_to_string(&cert_path)?;
        let key_pem = std::fs::read_to_string(&key_path)?;
        Ok((cert_pem, key_pem))
    } else {
        info!("Generating new CA certificate in {}", ca_dir.display());
        generate_ca(ca_dir)
    }
}

fn generate_ca(ca_dir: &Path) -> Result<(String, String), Error> {
    let key_pair = KeyPair::generate()?;

    let mut params = CertificateParams::default();
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);

    let mut dn = DistinguishedName::new();
    dn.push(DnType::CommonName, "Mac Proxy Cache CA");
    dn.push(DnType::OrganizationName, "Mac Proxy Cache");
    params.distinguished_name = dn;

    let cert = params.self_signed(&key_pair)?;
    let cert_pem = cert.pem();
    let key_pem = key_pair.serialize_pem();

    std::fs::create_dir_all(ca_dir)?;
    std::fs::write(ca_dir.join("ca.crt"), &cert_pem)?;
    std::fs::write(ca_dir.join("ca.key"), &key_pem)?;

    info!(
        "CA certificate written to {}",
        ca_dir.join("ca.crt").display()
    );

    Ok((cert_pem, key_pem))
}
