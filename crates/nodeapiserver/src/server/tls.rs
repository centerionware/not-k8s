//! TLS server certificate for the REST/watch listener. Self-signed,
//! generated on first start and persisted — the same posture
//! `crates/nodelet/src/server/tls.rs` already established for its own
//! HTTPS server, adapted here rather than reinvented.
//!
//! A standalone binary uses a persisted self-signed development certificate.
//! The nodebootstrap `nodeapiserver` target supplies the cluster's real
//! CA-signed PEM serving pair instead, just as real kube-apiserver accepts
//! `--tls-cert-file`/`--tls-private-key-file`; the generated certificate is
//! retained as a fallback for direct development runs.
//!
//! Client certificate verification is optional and off by default
//! (`with_no_client_auth()`, when `server_config` is called with
//! `client_ca: None`) — configuring `NODEAPISERVER_CLIENT_CA_FILE`
//! (`config::Config::client_ca_file`) turns it on, offered but not
//! required (`allow_unauthenticated()`), the exact same
//! client-CA-is-optional design `crates/nodelet/src/server/tls.rs`'s own
//! `server_config`/`load_client_ca` already established for its own HTTPS
//! server — mirrored here, not reinvented. `authn::x509::identity_from_der`
//! is what turns an accepted, verified peer certificate into an
//! `Identity`; this module only handles the TLS-layer verification.
//!
//! `ReloadableClientCa` refreshes the client trust bundle for new TLS
//! connections after a valid file replacement, retaining the last valid
//! bundle if a rotation is temporarily malformed.

use anyhow::{Context, Result};
use rcgen::{generate_simple_self_signed, CertifiedKey};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use rustls::server::WebPkiClientVerifier;
use rustls::{RootCertStore, ServerConfig};
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::SystemTime;
use tracing::warn;

pub struct LoadedCert {
    cert_der: CertificateDer<'static>,
    key_der: Vec<u8>,
}

/// A client-CA bundle that reloads after an atomic replacement or edit. The
/// listener builds a fresh TLS verifier for each accepted connection, so a
/// valid rotation takes effect without restarting nodeapiserver. Invalid or
/// temporarily incomplete contents retain the last valid trust store.
#[derive(Clone)]
pub struct ReloadableClientCa {
    path: PathBuf,
    state: Arc<RwLock<ClientCaState>>,
}

struct ClientCaState {
    fingerprint: FileFingerprint,
    store: RootCertStore,
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct FileFingerprint {
    modified: Option<SystemTime>,
    len: u64,
}

impl LoadedCert {
    /// Build the TLS server config. When `client_ca` is `Some`, client
    /// certificate authentication is offered but not required: a request
    /// presenting a cert that chains to `client_ca` is accepted at the TLS
    /// layer (the caller then reads its identity back out of the verified
    /// peer cert via `authn::x509::identity_from_der`); a request
    /// presenting no cert at all still completes the handshake. A cert
    /// that does NOT chain to `client_ca` fails the handshake outright,
    /// before any of this crate's own code runs — that's rustls's own
    /// verification, not ours.
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

/// Parses a PEM bundle of one or more CA certificates
/// (`NODEAPISERVER_CLIENT_CA_FILE`) into a `RootCertStore` for client
/// certificate verification. Same shape and error posture as
/// `crates/nodelet/src/server/tls.rs::load_client_ca`.
pub fn load_client_ca(path: &Path) -> Result<RootCertStore> {
    let pem_bytes = std::fs::read(path).with_context(|| format!("reading client CA file {}", path.display()))?;
    let mut store = RootCertStore::empty();
    let mut count = 0usize;
    for pem in x509_parser::pem::Pem::iter_from_buffer(&pem_bytes) {
        let pem = pem.context("parsing PEM block in client CA file")?;
        store.add(CertificateDer::from(pem.contents)).context("adding CA certificate to root store")?;
        count += 1;
    }
    if count == 0 {
        anyhow::bail!("client CA file {} contained no PEM certificates", path.display());
    }
    Ok(store)
}

impl ReloadableClientCa {
    /// Load a client CA bundle and retain its last valid contents during
    /// later malformed rotations.
    pub fn from_file(path: &Path) -> Result<Self> {
        let store = load_client_ca(path)?;
        let fingerprint = file_fingerprint(path)?;
        Ok(Self {
            path: path.to_path_buf(),
            state: Arc::new(RwLock::new(ClientCaState { fingerprint, store })),
        })
    }

    /// Return the latest valid root store, refreshing it when the source file
    /// has changed.
    pub fn current(&self) -> RootCertStore {
        self.refresh_if_needed();
        self.state
            .read()
            .map(|state| state.store.clone())
            .unwrap_or_else(|poisoned| poisoned.into_inner().store.clone())
    }

    fn refresh_if_needed(&self) {
        let Ok(fingerprint) = file_fingerprint(&self.path) else {
            return;
        };
        let needs_reload = self
            .state
            .read()
            .map(|state| state.fingerprint != fingerprint)
            .unwrap_or(false);
        if !needs_reload {
            return;
        }

        let store = match load_client_ca(&self.path) {
            Ok(store) => store,
            Err(error) => {
                if let Ok(mut state) = self.state.write() {
                    state.fingerprint = fingerprint;
                }
                warn!(path = %self.path.display(), error = ?error, "client CA bundle changed but could not be reloaded; retaining the last valid trust store");
                return;
            }
        };
        if let Ok(mut state) = self.state.write() {
            state.fingerprint = fingerprint;
            state.store = store;
        }
    }
}

fn file_fingerprint(path: &Path) -> Result<FileFingerprint> {
    let metadata = std::fs::metadata(path)
        .with_context(|| format!("reading client CA file metadata {}", path.display()))?;
    Ok(FileFingerprint {
        modified: metadata.modified().ok(),
        len: metadata.len(),
    })
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

/// Load the cluster bootstrapper's PEM certificate and PKCS#8 private key.
/// The standalone fallback above intentionally remains available for local
/// listener tests, but an installed apiserver must use the shared cluster CA
/// rather than minting a second trust root.
pub fn load_from_pem(cert_path: &Path, key_path: &Path) -> Result<LoadedCert> {
    let cert_pem = std::fs::read(cert_path)
        .with_context(|| format!("reading TLS certificate {}", cert_path.display()))?;
    let key_pem = std::fs::read(key_path)
        .with_context(|| format!("reading TLS private key {}", key_path.display()))?;
    let cert = x509_parser::pem::Pem::iter_from_buffer(&cert_pem)
        .next()
        .context("TLS certificate PEM file is empty")?
        .context("parsing TLS certificate PEM")?;
    anyhow::ensure!(!cert.contents.is_empty(), "TLS certificate PEM file contains an empty certificate");
    let key = x509_parser::pem::Pem::iter_from_buffer(&key_pem)
        .next()
        .context("TLS private key PEM file is empty")?
        .context("parsing TLS private key PEM")?;
    anyhow::ensure!(!key.contents.is_empty(), "TLS private key PEM file contains an empty key");
    Ok(LoadedCert {
        cert_der: CertificateDer::from(cert.contents),
        key_der: key.contents,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `crate::install_crypto_provider()` runs once at real process
    /// startup, but nothing calls it in the test binary — rustls needs one
    /// installed before `ServerConfig::builder()` will build anything.
    /// Ignoring the error (rather than calling `install_crypto_provider()`
    /// itself, which `expect()`s) is deliberate: whichever test in this
    /// binary runs first wins the race to install it, and every other
    /// test's own attempt would otherwise panic on the guaranteed-to-fail
    /// second call.
    fn ensure_crypto_provider() {
        let _ = rustls::crypto::ring::default_provider().install_default();
    }

    #[test]
    fn generates_and_reloads_a_cert_deterministically_from_disk() {
        ensure_crypto_provider();
        let dir = tempfile::tempdir().unwrap();
        let sans = vec!["localhost".to_string(), "127.0.0.1".to_string()];

        let first = load_or_generate(dir.path(), &sans).unwrap();
        let config = first.server_config(None);
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

    #[test]
    fn loads_a_bootstrap_pem_certificate_and_key() {
        ensure_crypto_provider();
        let dir = tempfile::tempdir().unwrap();
        let generated = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
        let cert_path = dir.path().join("server.crt");
        let key_path = dir.path().join("server.key");
        std::fs::write(&cert_path, generated.cert.pem()).unwrap();
        std::fs::write(&key_path, generated.key_pair.serialize_pem()).unwrap();

        let loaded = load_from_pem(&cert_path, &key_path).unwrap();
        assert!(loaded.server_config(None).is_ok());
    }

    fn self_signed_ca_pem(cn: &str) -> String {
        use rcgen::{CertificateParams, DistinguishedName, DnType, KeyPair};
        let mut dn = DistinguishedName::new();
        dn.push(DnType::CommonName, cn);
        let mut params = CertificateParams::default();
        params.distinguished_name = dn;
        let key_pair = KeyPair::generate().unwrap();
        let cert = params.self_signed(&key_pair).unwrap();
        cert.pem()
    }

    #[test]
    fn loads_a_pem_bundle_with_one_ca() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("client-ca.pem");
        std::fs::write(&path, self_signed_ca_pem("test-ca")).unwrap();

        let store = load_client_ca(&path).expect("a single valid PEM CA cert must load");
        assert_eq!(store.len(), 1);
    }

    #[test]
    fn an_empty_ca_file_is_a_real_error_not_a_silently_empty_store() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("empty-ca.pem");
        std::fs::write(&path, []).unwrap();
        assert!(load_client_ca(&path).is_err());
    }

    #[test]
    fn a_client_ca_enabled_config_still_builds_a_valid_server_config() {
        ensure_crypto_provider();
        let dir = tempfile::tempdir().unwrap();
        let sans = vec!["localhost".to_string()];
        let cert = load_or_generate(dir.path(), &sans).unwrap();

        let ca_path = dir.path().join("client-ca.pem");
        std::fs::write(&ca_path, self_signed_ca_pem("test-ca")).unwrap();
        let store = load_client_ca(&ca_path).unwrap();

        assert!(cert.server_config(Some(&store)).is_ok());
    }

    #[test]
    fn reloadable_client_ca_refreshes_and_retains_the_last_valid_bundle() {
        let dir = tempfile::tempdir().unwrap();
        let ca_path = dir.path().join("client-ca.pem");
        std::fs::write(&ca_path, self_signed_ca_pem("first-ca")).unwrap();
        let client_ca = ReloadableClientCa::from_file(&ca_path).unwrap();
        assert_eq!(client_ca.current().len(), 1);

        let replacement = format!(
            "{}{}",
            self_signed_ca_pem("second-ca"),
            self_signed_ca_pem("third-ca")
        );
        std::fs::write(&ca_path, replacement).unwrap();
        assert_eq!(client_ca.current().len(), 2);

        std::fs::write(&ca_path, "not a certificate").unwrap();
        assert_eq!(client_ca.current().len(), 2);
    }
}
