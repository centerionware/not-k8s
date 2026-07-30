//! Self-signed TLS server certificate for the kubelet-style HTTP(S)
//! server. Real kubelet does the same thing by default (a self-signed cert
//! it generates itself) unless a real CA/CSR pipeline is configured — see
//! docs/GAP_CLOSURE.md for that as a still-open, lower-priority item.
//! Persisted to disk as raw DER (not PEM — avoids pulling in a PEM-parsing
//! dependency just to read back two files nodelet wrote itself) so restarts
//! don't invalidate a client that already trusts/pinned the cert.

use anyhow::{Context, Result};
use rcgen::{generate_simple_self_signed, CertifiedKey};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use rustls::ServerConfig;
use std::path::Path;
use tracing::warn;

pub struct LoadedCert {
    cert_der: CertificateDer<'static>,
    key_der: Vec<u8>,
}

impl LoadedCert {
    pub fn server_config(&self) -> ServerConfig {
        let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(self.key_der.clone()));
        ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(vec![self.cert_der.clone()], key)
            .expect("building rustls ServerConfig from a valid DER cert/key must succeed")
    }
}

pub fn load_or_generate(cert_dir: &str, node_name: &str) -> Result<LoadedCert> {
    let dir = Path::new(cert_dir);
    let cert_path = dir.join("server.crt.der");
    let key_path = dir.join("server.key.der");

    if let (Ok(cert_bytes), Ok(key_bytes)) = (std::fs::read(&cert_path), std::fs::read(&key_path)) {
        if !cert_bytes.is_empty() && !key_bytes.is_empty() {
            return Ok(LoadedCert { cert_der: CertificateDer::from(cert_bytes), key_der: key_bytes });
        }
        warn!(dir = %cert_dir, "existing TLS cert/key files are empty; regenerating");
    }

    std::fs::create_dir_all(dir).with_context(|| format!("creating server cert directory {cert_dir}"))?;
    let sans = vec![node_name.to_string(), "localhost".to_string()];
    let CertifiedKey { cert, key_pair } =
        generate_simple_self_signed(sans).context("generating self-signed TLS certificate")?;
    let cert_der = cert.der().clone();
    let key_der = key_pair.serialize_der();

    std::fs::write(&cert_path, cert_der.as_ref()).context("writing server.crt.der")?;
    std::fs::write(&key_path, &key_der).context("writing server.key.der")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&key_path, std::fs::Permissions::from_mode(0o600));
    }

    Ok(LoadedCert { cert_der, key_der })
}

#[cfg(test)]
#[path = "tls_tests/load_or_generate.rs"]
mod tests_load_or_generate;
