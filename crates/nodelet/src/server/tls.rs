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
use rustls::server::WebPkiClientVerifier;
use rustls::{RootCertStore, ServerConfig};
use std::path::Path;
use std::sync::Arc;
use tracing::warn;

pub struct LoadedCert {
    cert_der: CertificateDer<'static>,
    key_der: Vec<u8>,
}

impl LoadedCert {
    /// Build the TLS server config. When `client_ca` is `Some`, client
    /// certificate authentication is offered but not required
    /// (`allow_unauthenticated()`): a request presenting a cert that chains
    /// to `client_ca` is accepted at the TLS layer (nodelet then reads its
    /// identity back out of the verified peer cert — see
    /// `client_identity_from_der` below); a request presenting no cert at
    /// all still completes the handshake and falls back to the existing
    /// bearer-token path in `routes::handle`. A cert that does NOT chain to
    /// `client_ca` fails the handshake outright, before any nodelet code
    /// runs — that's rustls's own verification, not ours.
    pub fn server_config(&self, client_ca: Option<&RootCertStore>) -> Result<ServerConfig> {
        let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(self.key_der.clone()));
        let builder = match client_ca {
            Some(store) => {
                let verifier = WebPkiClientVerifier::builder(Arc::new(store.clone()))
                    .allow_unauthenticated()
                    .build()
                    .context("building client certificate verifier")?;
                ServerConfig::builder().with_client_cert_verifier(verifier)
            }
            None => ServerConfig::builder().with_no_client_auth(),
        };
        builder
            .with_single_cert(vec![self.cert_der.clone()], key)
            .context("building rustls ServerConfig from a valid DER cert/key")
    }
}

/// Parse a PEM bundle of one or more CA certificates (`NODELET_CLIENT_CA_FILE`)
/// into a `RootCertStore` for client certificate verification.
pub fn load_client_ca(path: &str) -> Result<RootCertStore> {
    let pem_bytes = std::fs::read(path).with_context(|| format!("reading client CA file {path}"))?;
    let mut store = RootCertStore::empty();
    let mut count = 0usize;
    for pem in x509_parser::pem::Pem::iter_from_buffer(&pem_bytes) {
        let pem = pem.context("parsing PEM block in client CA file")?;
        store
            .add(CertificateDer::from(pem.contents))
            .context("adding CA certificate to root store")?;
        count += 1;
    }
    if count == 0 {
        anyhow::bail!("client CA file {path} contained no PEM certificates");
    }
    Ok(store)
}

/// Extract the identity (username, groups) a real Kubernetes apiserver's own
/// generic x509 authenticator would derive from this leaf certificate: the
/// Subject Common Name as the username, Subject Organization values as
/// groups. `None` if the cert can't be parsed or has no CN.
pub fn client_identity_from_der(der: &[u8]) -> Option<(String, Vec<String>)> {
    let (_, cert) = x509_parser::parse_x509_certificate(der).ok()?;
    let subject = cert.subject();
    let username = subject.iter_common_name().next()?.as_str().ok()?.to_string();
    let groups = subject
        .iter_organization()
        .filter_map(|o| o.as_str().ok().map(str::to_string))
        .collect();
    Some((username, groups))
}

pub fn load_or_generate(cert_dir: &str, node_name: &str, node_ip: &str) -> Result<LoadedCert> {
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
    // node_name/"localhost" alone leave this cert with zero IP SANs — the
    // apiserver dials nodelet's server by the node's *address*
    // (Node.status.addresses' InternalIP, same one node.rs advertises via
    // detect_internal_ip()), not its hostname, for exec/logs/attach/
    // port-forward proxying. Without the IP itself in here, that dial
    // fails TLS verification outright ("doesn't contain any IP SANs"),
    // confirmed for real against a live k3s apiserver — every one of
    // those four endpoints was unreachable through the apiserver proxy
    // despite the server itself working fine. generate_simple_self_signed
    // parses each string and adds it as a DNS name or IP SAN as
    // appropriate — a literal IP address string here becomes a real
    // SanType::IpAddress entry, no extra API needed.
    let sans = vec![node_name.to_string(), "localhost".to_string(), node_ip.to_string(), "127.0.0.1".to_string()];
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
#[cfg(test)]
#[path = "tls_tests/client_cert_auth.rs"]
mod tests_client_cert_auth;
