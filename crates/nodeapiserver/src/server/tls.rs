//! TLS server certificate for the REST/watch listener. Self-signed,
//! generated on first start and persisted — the same posture
//! `crates/nodelet/src/server/tls.rs` already established for its own
//! HTTPS server, adapted here rather than reinvented.
//!
//! **This is not the cluster's real PKI.** A production cluster needs
//! kubectl/client-go to actually trust this certificate, which needs a
//! real CA distributed as part of cluster bootstrap — that's Group O's
//! job (`docs/APISERVER.md`: "cluster PKI generation (CA, serving cert,
//! ...)"). This module gets Group E's listener running and testable now;
//! Group O is expected to replace or wire real PKI material into it later,
//! the same way real kube-apiserver's own `--tls-cert-file`/
//! `--tls-private-key-file` flags let an operator supply real material
//! instead of the generated fallback.
//!
//! No client certificate verification yet (`with_no_client_auth()`) —
//! that's authn's job (Group H), same layering nodelet's own module
//! doc comment describes for its client-CA-is-optional design.

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
    pub fn server_config(&self) -> Result<ServerConfig> {
        let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(self.key_der.clone()));
        ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(vec![self.cert_der.clone()], key)
            .context("building rustls ServerConfig from a valid DER cert/key")
    }
}

/// Loads a cert/key pair from `cert_dir` if present, else generates a
/// self-signed one and persists it there — same DER-not-PEM choice
/// nodelet's own `load_or_generate` makes (avoids a PEM-parsing dependency
/// just to read back two files this process wrote itself; a client that
/// wants to trust this cert reads the *server's* own certificate off the
/// wire during the handshake, not by re-parsing this file).
pub fn load_or_generate(cert_dir: &Path, sans: &[String]) -> Result<LoadedCert> {
    let cert_path = cert_dir.join("server.crt.der");
    let key_path = cert_dir.join("server.key.der");

    if let (Ok(cert_bytes), Ok(key_bytes)) = (std::fs::read(&cert_path), std::fs::read(&key_path)) {
        if !cert_bytes.is_empty() && !key_bytes.is_empty() {
            return Ok(LoadedCert { cert_der: CertificateDer::from(cert_bytes), key_der: key_bytes });
        }
        warn!(dir = %cert_dir.display(), "existing TLS cert/key files are empty; regenerating");
    }

    std::fs::create_dir_all(cert_dir).with_context(|| format!("creating server cert directory {}", cert_dir.display()))?;
    let CertifiedKey { cert, key_pair } =
        generate_simple_self_signed(sans.to_vec()).context("generating self-signed TLS certificate")?;
    let cert_der = cert.der().clone();
    let key_der = key_pair.serialize_der();

    std::fs::write(&cert_path, cert_der.as_ref()).context("writing server.crt.der")?;
    std::fs::write(&key_path, &key_der).context("writing server.key.der")?;
    Ok(LoadedCert { cert_der, key_der })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generates_and_reloads_a_cert_deterministically_from_disk() {
        let dir = tempfile::tempdir().unwrap();
        let sans = vec!["localhost".to_string(), "127.0.0.1".to_string()];

        let first = load_or_generate(dir.path(), &sans).unwrap();
        let config = first.server_config();
        assert!(config.is_ok(), "a freshly generated cert must build a valid ServerConfig");

        // Second call must load the same material back off disk, not
        // silently regenerate a different cert every restart (which would
        // break every client that had pinned/trusted the previous one).
        let second = load_or_generate(dir.path(), &sans).unwrap();
        assert_eq!(first.cert_der.as_ref(), second.cert_der.as_ref());
        assert_eq!(first.key_der, second.key_der);
    }

    #[test]
    fn empty_existing_files_trigger_regeneration_rather_than_a_hard_failure() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("server.crt.der"), []).unwrap();
        std::fs::write(dir.path().join("server.key.der"), []).unwrap();
        let cert = load_or_generate(dir.path(), &["localhost".to_string()]).unwrap();
        assert!(!cert.cert_der.is_empty());
    }
}
