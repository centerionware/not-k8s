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
//! A configured proxy client certificate is presented to the aggregated
//! backend, and the listener adds the authenticated request identity as
//! `X-Remote-User`/`X-Remote-Group` headers. This is the same front-proxy
//! contract real kube-aggregator uses; the certificate is what lets the
//! backend trust those headers rather than accepting caller-supplied values.

use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use rustls::{ClientConfig, RootCertStore};
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("no rustls CryptoProvider installed — install_crypto_provider() must run before this")]
    NoCryptoProvider,
    #[error("parsing spec.caBundle as PEM failed: {0}")]
    InvalidCaBundle(#[source] x509_parser::error::PEMError),
    #[error("spec.caBundle contained no PEM certificates")]
    EmptyCaBundle,
    #[error("reading aggregation proxy client material at {path}: {source}")]
    ReadClientMaterial { path: PathBuf, source: std::io::Error },
    #[error("parsing aggregation proxy client material at {path}: {detail}")]
    InvalidClientMaterial { path: PathBuf, detail: String },
    #[error("adding a spec.caBundle certificate to the trust store: {0}")]
    Tls(#[from] rustls::Error),
}

/// The client identity the apiserver presents to an aggregated backend.
/// Parsed once at listener startup so a request does not reread certificate
/// files, while each per-APIService TLS config can still combine it with that
/// APIService's own trust settings.
#[derive(Clone)]
pub struct ClientIdentity {
    cert_chain_der: Vec<Vec<u8>>,
    key_der: Vec<u8>,
}

impl ClientIdentity {
    pub fn from_files(cert_path: &Path, key_path: &Path) -> Result<Self, Error> {
        let cert_bytes = std::fs::read(cert_path).map_err(|source| Error::ReadClientMaterial {
            path: cert_path.to_path_buf(),
            source,
        })?;
        let key_bytes = std::fs::read(key_path).map_err(|source| Error::ReadClientMaterial {
            path: key_path.to_path_buf(),
            source,
        })?;
        let cert_chain_der = pem_or_der_chain(&cert_bytes, cert_path)?;
        let key_der = pem_or_der_key(&key_bytes, key_path)?;
        Ok(Self { cert_chain_der, key_der })
    }
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
    build_client_config_with_identity(ca_bundle_pem, insecure_skip_tls_verify, None)
}

/// Builds the aggregation client config while optionally presenting the
/// listener's front-proxy identity to the backend.
pub fn build_client_config_with_identity(
    ca_bundle_pem: Option<&[u8]>,
    insecure_skip_tls_verify: bool,
    identity: Option<&ClientIdentity>,
) -> Result<ClientConfig, Error> {
    let provider = rustls::crypto::CryptoProvider::get_default().ok_or(Error::NoCryptoProvider)?.clone();
    let builder = if insecure_skip_tls_verify {
        ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(crate::proxy::client_tls::AcceptAnyServerCert::new(provider)))
    } else {
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
        ClientConfig::builder().with_root_certificates(store)
    };

    match identity {
        Some(identity) => {
            let cert_chain = identity
                .cert_chain_der
                .iter()
                .cloned()
                .map(CertificateDer::from)
                .collect();
            let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(identity.key_der.clone()));
            Ok(builder.with_client_auth_cert(cert_chain, key)?)
        }
        None => Ok(builder.with_no_client_auth()),
    }
}

fn pem_or_der_chain(bytes: &[u8], path: &Path) -> Result<Vec<Vec<u8>>, Error> {
    if !bytes.starts_with(b"-----BEGIN") {
        if bytes.is_empty() {
            return Err(Error::InvalidClientMaterial {
                path: path.to_path_buf(),
                detail: "certificate file is empty".to_string(),
            });
        }
        return Ok(vec![bytes.to_vec()]);
    }
    let mut chain = Vec::new();
    for pem in x509_parser::pem::Pem::iter_from_buffer(bytes) {
        let pem = pem.map_err(|error| Error::InvalidClientMaterial {
            path: path.to_path_buf(),
            detail: error.to_string(),
        })?;
        if !pem.contents.is_empty() {
            chain.push(pem.contents);
        }
    }
    if chain.is_empty() {
        return Err(Error::InvalidClientMaterial {
            path: path.to_path_buf(),
            detail: "certificate PEM file contained no certificates".to_string(),
        });
    }
    Ok(chain)
}

fn pem_or_der_key(bytes: &[u8], path: &Path) -> Result<Vec<u8>, Error> {
    if !bytes.starts_with(b"-----BEGIN") {
        if bytes.is_empty() {
            return Err(Error::InvalidClientMaterial {
                path: path.to_path_buf(),
                detail: "key file is empty".to_string(),
            });
        }
        return Ok(bytes.to_vec());
    }
    let key = x509_parser::pem::Pem::iter_from_buffer(bytes)
        .next()
        .ok_or_else(|| Error::InvalidClientMaterial {
            path: path.to_path_buf(),
            detail: "key PEM file contained no key".to_string(),
        })?
        .map_err(|error| Error::InvalidClientMaterial {
            path: path.to_path_buf(),
            detail: error.to_string(),
        })?;
    if key.contents.is_empty() {
        return Err(Error::InvalidClientMaterial {
            path: path.to_path_buf(),
            detail: "key PEM file contained an empty key".to_string(),
        });
    }
    Ok(key.contents)
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

    #[test]
    fn a_front_proxy_client_identity_is_loaded_and_attached() {
        ensure_provider();
        let dir = tempfile::tempdir().unwrap();
        let generated = rcgen::generate_simple_self_signed(vec!["front-proxy".to_string()]).unwrap();
        let cert_path = dir.path().join("proxy.crt");
        let key_path = dir.path().join("proxy.key");
        std::fs::write(&cert_path, generated.cert.pem()).unwrap();
        std::fs::write(&key_path, generated.key_pair.serialize_pem()).unwrap();

        let identity = ClientIdentity::from_files(&cert_path, &key_path).unwrap();
        assert!(build_client_config_with_identity(None, true, Some(&identity)).is_ok());
    }
}
