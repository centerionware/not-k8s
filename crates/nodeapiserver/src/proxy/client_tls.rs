//! TLS client configuration for talking to nodelet's own kubelet-style
//! server (`crates/nodelet/src/server/tls.rs`) — the other half of the
//! mTLS relationship that module's own client-cert-auth path
//! (`NODELET_CLIENT_CA_FILE`) already established. Building the actual
//! live proxy (`proxy::http_client`) needs a real `rustls::ClientConfig`
//! to dial with; this module is just that config, no I/O of its own.
//!
//! # Server certificate verification
//!
//! **Deliberately insecure by default, and this is the faithful choice,
//! not a shortcut**: real upstream's own `KubeletClientConfig` behaves
//! identically when `--kubelet-certificate-authority` isn't configured
//! — `transportConfig()`
//! (`pkg/kubelet/client/kubelet_client.go`, fetched and read directly)
//! sets `cfg.TLS.Insecure = true` whenever `!cfg.HasCA()`, i.e. real
//! kube-apiserver skips verifying kubelet's serving certificate
//! entirely unless an operator explicitly configures a CA for it. This
//! module's own `AcceptAnyServerCert` mirrors that real default exactly
//! — it still cryptographically verifies the TLS handshake signature
//! (a real man-in-the-middle without the private key still can't
//! complete the handshake), it just never checks the presented
//! certificate chains to any particular trusted root.
//!
//! # Client certificate
//!
//! Optional — `NODEAPISERVER_KUBELET_CLIENT_CERT_FILE`/`_KEY_FILE`
//! (`config::Config`), raw DER (not PEM), matching
//! `crates/nodelet/src/server/tls.rs`'s own persisted-as-DER convention
//! exactly (avoids pulling in a PEM-parsing dependency this workspace
//! doesn't otherwise need). When set, nodelet's own
//! `NODELET_CLIENT_CA_FILE` x509-authenticates this connection directly
//! — no `TokenReview` round trip (see `crates/nodelet/src/server/
//! routes.rs`'s own doc comment on that path). When unset, nodelet
//! falls back to its own bearer-token path, which this build has no
//! credential for yet — a real, separate, not-yet-solved problem named
//! in `docs/APISERVER.md`'s own Group N section.

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime};
use rustls::{ClientConfig, DigitallySignedStruct, Error as TlsError, SignatureScheme};
use std::sync::Arc;

/// Real upstream's own default posture when no kubelet CA is configured
/// — see this module's own doc comment. Still cryptographically
/// verifies the handshake signature via the process's installed
/// `CryptoProvider` (`crate::install_crypto_provider`, already called at
/// startup); only the certificate-chain-to-a-trusted-root check is
/// skipped.
#[derive(Debug)]
pub(crate) struct AcceptAnyServerCert {
    provider: Arc<rustls::crypto::CryptoProvider>,
}

impl AcceptAnyServerCert {
    /// `pub(crate)` rather than private: `aggregator::client_tls`'s own
    /// `spec.insecureSkipTLSVerify: true` posture is the exact same real
    /// verifier this module already built for nodelet's own default trust
    /// posture -- reused directly rather than duplicated.
    pub(crate) fn new(provider: Arc<rustls::crypto::CryptoProvider>) -> Self {
        AcceptAnyServerCert { provider }
    }
}

impl ServerCertVerifier for AcceptAnyServerCert {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, TlsError> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(&self, message: &[u8], cert: &CertificateDer<'_>, dss: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, TlsError> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.provider.signature_verification_algorithms)
    }

    fn verify_tls13_signature(&self, message: &[u8], cert: &CertificateDer<'_>, dss: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, TlsError> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.provider.signature_verification_algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider.signature_verification_algorithms.supported_schemes()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("no rustls CryptoProvider installed — install_crypto_provider() must run before this")]
    NoCryptoProvider,
    #[error("reading client cert/key material at {path}: {source}")]
    ReadMaterial { path: std::path::PathBuf, source: std::io::Error },
    #[error("building the TLS client config: {0}")]
    Tls(#[from] rustls::Error),
}

/// Builds the `rustls::ClientConfig` [`crate::proxy::http_client`] dials
/// nodelet with. `client_cert_key` is `Some((cert_der_path,
/// key_der_path))` when both `NODEAPISERVER_KUBELET_CLIENT_CERT_FILE`/
/// `_KEY_FILE` are configured — `None` connects with no client identity
/// at all (nodelet then falls back to its own bearer-token path, which
/// this build can't satisfy yet, so such a request will get a real
/// `401` from nodelet itself, not a silent failure here).
pub fn build_client_config(client_cert_key: Option<(&std::path::Path, &std::path::Path)>) -> Result<ClientConfig, Error> {
    let provider = rustls::crypto::CryptoProvider::get_default().ok_or(Error::NoCryptoProvider)?.clone();
    let builder = ClientConfig::builder().dangerous().with_custom_certificate_verifier(Arc::new(AcceptAnyServerCert { provider }));

    let config = match client_cert_key {
        Some((cert_path, key_path)) => {
            let cert_der = std::fs::read(cert_path).map_err(|source| Error::ReadMaterial { path: cert_path.to_path_buf(), source })?;
            let key_der = std::fs::read(key_path).map_err(|source| Error::ReadMaterial { path: key_path.to_path_buf(), source })?;
            let cert_chain = vec![CertificateDer::from(cert_der)];
            let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key_der));
            builder.with_client_auth_cert(cert_chain, key)?
        }
        None => builder.with_no_client_auth(),
    };
    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ensure_provider() {
        let _ = rustls::crypto::ring::default_provider().install_default();
    }

    #[test]
    fn builds_a_client_config_with_no_client_cert() {
        ensure_provider();
        assert!(build_client_config(None).is_ok());
    }

    #[test]
    fn a_missing_client_cert_file_is_a_named_read_error() {
        ensure_provider();
        let bogus = std::path::Path::new("/nonexistent/cert.der");
        let err = build_client_config(Some((bogus, bogus))).expect_err("a missing file must be a clear error");
        assert!(matches!(err, Error::ReadMaterial { .. }), "got {err:?}");
    }
}
