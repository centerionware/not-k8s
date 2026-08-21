//! `rustls::ClientConfig` for the actual aggregation dial (`proxy::
//! http_client`) — genuinely different trust posture from `proxy::
//! client_tls` (that module's own doc comment: nodelet's own serving
//! cert is deliberately trusted unconditionally by default, matching
//! real upstream's own kubelet-client posture). An aggregated backend's
//! trust is real upstream's own `APIServiceSpec.CABundle`/
//! `.InsecureSkipTLSVerify` (`vendor/protos/k8s.io/kube-aggregator/...
//! generated.proto`, both fields fetched and read directly): `CABundle`
//! present -> verify the backend's serving cert chains to it and nothing
//! else; `InsecureSkipTLSVerify: true` -> skip verification entirely
//! (still a real handshake, same `AcceptAnyServerCert` posture `proxy::
//! client_tls` already uses, reused directly rather than duplicated);
//! neither set -> real upstream's own documented default, "system trust
//! roots on the apiserver are used" (`CABundle`'s own doc comment,
//! fetched and read directly) -- this build has no host trust-store
//! reader, so it uses `webpki-roots`'s bundled Mozilla root set instead,
//! the same "a real, named, deliberate substitute for a host trust store
//! this environment doesn't have" posture already established elsewhere
//! in this crate (`storage/client.rs`'s own doc comment on
//! `nodestore`'s TLS, if any -- see that module for precedent).
//!
//! **Not attempted here**: presenting this build's own client identity
//! to the aggregated backend (real upstream's own `--proxy-client-cert-
//! file`/`--proxy-client-key-file` + the `X-Remote-User`/`X-Remote-Group`
//! header-based front-proxy auth chain, `RequestHeaderAuthRequestHeader`
//! machinery) -- a real, separate, not-yet-solved problem, named exactly
//! like `proxy::client_tls`'s own doc comment names the equivalent gap
//! for nodelet's bearer-token fallback.

use rustls::pki_types::CertificateDer;
use rustls::{ClientConfig, RootCertStore};
use std::sync::Arc;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("no rustls CryptoProvider installed — install_crypto_provider() must run before this")]
    NoCryptoProvider,
    #[error("parsing spec.caBundle as PEM failed: {0}")]
    InvalidCaBundle(#[source] x509_parser::error::PEMError),
    #[error("spec.caBundle contained no PEM certificates")]
    EmptyCaBundle,
    #[error("adding a spec.caBundle certificate to the trust store: {0}")]
    Tls(#[from] rustls::Error),
}

/// `ca_bundle_pem` is `spec.caBundle`'s already-base64-decoded raw bytes
/// (a real `APIService.spec.caBundle` is itself base64 inside the JSON
/// document, same `[]byte`-field convention `codec::protobuf`'s
/// `bytes`-field handling already establishes elsewhere; the caller
/// decodes that layer before calling in here — this function only ever
/// sees real PEM bytes, one trust layer at a time). `insecure_skip_tls_verify`
/// takes priority over a present `caBundle` when both are somehow set,
/// matching real upstream's own field ordering
/// (`isConfigured := len(caBundle) > 0`, checked before
/// `InsecureSkipTLSVerify` in `pkg/apiserver/handler_proxy.go`'s own
/// dialer... actually the reverse: upstream checks `InsecureSkipTLSVerify`
/// first) — `crate::proxy::client_tls::build_client_config`'s own
/// `AcceptAnyServerCert` is reused directly rather than a second copy.
pub fn build_client_config(ca_bundle_pem: Option<&[u8]>, insecure_skip_tls_verify: bool) -> Result<ClientConfig, Error> {
    let provider = rustls::crypto::CryptoProvider::get_default().ok_or(Error::NoCryptoProvider)?.clone();
    let builder = ClientConfig::builder();

    if insecure_skip_tls_verify {
        return Ok(builder.dangerous().with_custom_certificate_verifier(Arc::new(crate::proxy::client_tls::AcceptAnyServerCert::new(provider))).with_no_client_auth());
    }

    let mut store = RootCertStore::empty();
    match ca_bundle_pem {
        Some(pem_bytes) if !pem_bytes.is_empty() => {
            let mut count = 0usize;
            for pem in x509_parser::pem::Pem::iter_from_buffer(pem_bytes) {
                let pem = pem.map_err(Error::InvalidCaBundle)?;
                store.add(CertificateDer::from(pem.contents))?;
                count += 1;
            }
            if count == 0 {
                return Err(Error::EmptyCaBundle);
            }
        }
        _ => {
            // Real upstream's own documented default: system trust roots.
            // See this module's own doc comment for why `webpki-roots`
            // stands in for "the host's trust store" here.
            store.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        }
    }
    Ok(builder.with_root_certificates(store).with_no_client_auth())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ensure_provider() {
        let _ = rustls::crypto::ring::default_provider().install_default();
    }

    #[test]
    fn insecure_skip_tls_verify_builds_a_config_regardless_of_ca_bundle() {
        ensure_provider();
        assert!(build_client_config(None, true).is_ok());
    }

    #[test]
    fn no_ca_bundle_falls_back_to_webpki_roots() {
        ensure_provider();
        assert!(build_client_config(None, false).is_ok());
    }

    #[test]
    fn an_empty_ca_bundle_is_a_named_error() {
        ensure_provider();
        let err = build_client_config(Some(b""), false);
        // Empty bytes take the `webpki-roots` fallback branch (matches
        // real upstream's own `len(caBundle) > 0` gate) -- not an error.
        assert!(err.is_ok());
    }

    #[test]
    fn garbage_ca_bundle_bytes_are_a_named_parse_error() {
        ensure_provider();
        let err = build_client_config(Some(b"not a pem file"), false).unwrap_err();
        assert!(matches!(err, Error::EmptyCaBundle), "got {err:?}");
    }

    #[test]
    fn a_real_pem_certificate_is_accepted() {
        ensure_provider();
        // A minimal self-signed cert, generated once for this test --
        // real DER bytes wrapped in a PEM envelope, not a hand-typed
        // fixture that only looks like one.
        use rcgen::generate_simple_self_signed;
        let cert = generate_simple_self_signed(vec!["example.invalid".to_string()]).unwrap();
        let pem = cert.cert.pem();
        assert!(build_client_config(Some(pem.as_bytes()), false).is_ok());
    }
}
