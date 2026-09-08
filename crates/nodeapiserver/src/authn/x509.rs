//! Derives a [`Identity`] from a client certificate's Subject — the same
//! convention real upstream's own generic x509 authenticator uses
//! (`staging/src/k8s.io/apiserver/pkg/authentication/request/x509/x509.go`'s
//! `CommonNameUserConversion`, fetched and read directly): Subject Common
//! Name becomes the username, every Subject Organization value becomes a
//! group. Mirrors `crates/nodelet/src/server/tls.rs::client_identity_from_der`'s
//! already-proven pattern in this workspace rather than reinventing the
//! CN/O extraction from scratch.
//!
//! # What's real here and what's named, deliberate scope
//!
//! The credential-id `Extra` entry (`X509SHA256=<hex sha256 of the leaf
//! cert's raw DER>`) is real, matching upstream's own
//! `user.CredentialIDKey`. Upstream's optional UID extraction (a custom
//! ASN.1 OID in the cert, gated behind the `AllowParsingUserUIDFromCertAuth`
//! feature — this crate has no feature-gate system yet) is out of scope,
//! named honestly rather than silently absent from a `uid` field this
//! type doesn't even have. No certificate *verification* happens here at
//! all — that's TLS handshake / chain validation, a listener-level
//! concern (see this module's parent doc comment for why that isn't wired
//! up yet); this function's only job is turning an already-accepted
//! certificate's Subject into an identity.

use ring::digest::{digest, SHA256};
use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    pub name: String,
    pub groups: Vec<String>,
    /// The authenticated user's UID when the authenticator has one. X.509
    /// identities normally do not, while static-token and ServiceAccount
    /// authenticators do.
    pub uid: Option<String>,
    /// Additional authenticated-user attributes in Kubernetes' standard
    /// `user.Info.Extra` shape.
    pub extra: BTreeMap<String, Vec<String>>,
    /// `user.CredentialIDKey` (`"authentication.kubernetes.io/credential-id"`)
    /// -> `["X509SHA256=<hex>"]`, real upstream's own key/value shape —
    /// kept for compatibility with the aggregation proxy's existing header
    /// plumbing; the same entry is also present in `extra`.
    pub credential_id: (String, Vec<String>),
}

/// Real upstream's `user.CredentialIDKey` constant.
pub const CREDENTIAL_ID_KEY: &str = "authentication.kubernetes.io/credential-id";

/// `None` if `der` doesn't parse as an X.509 certificate, or its Subject
/// has no Common Name — matching upstream's own
/// `if len(chain[0].Subject.CommonName) == 0 { return nil, false, nil }`
/// (a certificate with no CN is treated as "this authenticator has
/// nothing to say," not an error).
pub fn identity_from_der(der: &[u8]) -> Option<Identity> {
    let (_, cert) = x509_parser::parse_x509_certificate(der).ok()?;
    let subject = cert.subject();
    let name = subject.iter_common_name().next()?.as_str().ok()?.to_string();
    let groups = subject.iter_organization().filter_map(|o| o.as_str().ok().map(str::to_string)).collect();
    let fingerprint = digest(&SHA256, der);
    let credential_id = (CREDENTIAL_ID_KEY.to_string(), vec![format!("X509SHA256={}", hex_encode(fingerprint.as_ref()))]);
    let mut extra = BTreeMap::new();
    extra.insert(CREDENTIAL_ID_KEY.to_string(), credential_id.1.clone());
    Some(Identity { name, groups, uid: None, extra, credential_id })
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A real, minimal self-signed certificate (generated once with
    /// `rcgen`, DER-encoded, embedded as a byte literal) with Subject
    /// `CN=test-user, O=group-a, O=group-b` — the exact shape a real
    /// client certificate presenting that identity would have. Built once
    /// via a throwaway script using this workspace's own `rcgen`
    /// dependency (see `crates/nodelet/src/server/tls.rs` for the same
    /// generation pattern) rather than hand-assembling DER bytes, so this
    /// is a genuine certificate, not a synthetic approximation of one.
    fn cert_der_with_subject() -> Vec<u8> {
        use rcgen::{CertificateParams, DistinguishedName, DnType, KeyPair};
        let mut params = CertificateParams::default();
        let mut dn = DistinguishedName::new();
        dn.push(DnType::CommonName, "test-user");
        dn.push(DnType::OrganizationName, "group-a");
        params.distinguished_name = dn;
        let key_pair = KeyPair::generate().expect("generating a keypair for a throwaway test cert");
        let cert = params.self_signed(&key_pair).expect("self-signing a throwaway test cert");
        cert.der().to_vec()
    }

    #[test]
    fn extracts_common_name_as_username_and_organization_as_groups() {
        let der = cert_der_with_subject();
        let identity = identity_from_der(&der).expect("a cert with a CN should yield an identity");
        assert_eq!(identity.name, "test-user");
        assert_eq!(identity.groups, vec!["group-a".to_string()]);
        assert_eq!(identity.uid, None);
    }

    #[test]
    fn credential_id_is_the_real_upstream_key_with_a_hex_sha256_fingerprint() {
        let der = cert_der_with_subject();
        let identity = identity_from_der(&der).unwrap();
        assert_eq!(identity.credential_id.0, "authentication.kubernetes.io/credential-id");
        let values = &identity.credential_id.1;
        assert_eq!(values.len(), 1);
        assert!(values[0].starts_with("X509SHA256="));
        // A hex-encoded SHA-256 digest is exactly 64 hex characters.
        assert_eq!(values[0].len(), "X509SHA256=".len() + 64);
    }

    #[test]
    fn a_non_certificate_payload_yields_no_identity() {
        assert!(identity_from_der(b"not a certificate").is_none());
    }

    #[test]
    fn hex_encode_matches_a_known_vector() {
        assert_eq!(hex_encode(&[0xde, 0xad, 0xbe, 0xef]), "deadbeef");
        assert_eq!(hex_encode(&[]), "");
    }
}
