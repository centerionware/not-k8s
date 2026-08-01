//! Client certificate identity extraction + CA-bundle loading (round 95).
//! Uses real rcgen-generated certs (not mocks) — same discipline as
//! `load_or_generate.rs`: this is exactly what runs against a real client
//! cert at handshake time.
use super::*;
use rcgen::{CertificateParams, DistinguishedName, DnType, KeyPair};

fn ensure_crypto_provider() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

fn self_signed_cert(cn: &str, orgs: &[&str]) -> Vec<u8> {
    ensure_crypto_provider();
    let mut dn = DistinguishedName::new();
    dn.push(DnType::CommonName, cn);
    for org in orgs {
        dn.push(DnType::OrganizationName, *org);
    }
    let mut params = CertificateParams::default();
    params.distinguished_name = dn;
    let key_pair = KeyPair::generate().unwrap();
    let cert = params.self_signed(&key_pair).unwrap();
    cert.der().to_vec()
}

#[test]
fn extracts_username_from_common_name() {
    let der = self_signed_cert("system:node:edge-1", &[]);
    let (username, groups) = client_identity_from_der(&der).expect("must parse a valid cert");
    assert_eq!(username, "system:node:edge-1");
    assert!(groups.is_empty());
}

#[test]
fn extracts_groups_from_organization_values() {
    // rcgen's DistinguishedName only supports one value per attribute type
    // (it's backed by a HashMap<DnType, DnValue>), so a single Organization
    // value is the most this test harness can produce — but that's still
    // enough to prove the Organization -> groups extraction path works.
    let der = self_signed_cert("alice", &["system:masters"]);
    let (username, groups) = client_identity_from_der(&der).expect("must parse a valid cert");
    assert_eq!(username, "alice");
    assert_eq!(groups, vec!["system:masters".to_string()]);
}

#[test]
fn returns_none_for_garbage_bytes() {
    assert!(client_identity_from_der(&[1, 2, 3, 4]).is_none());
}

#[test]
fn returns_none_for_a_cert_with_no_common_name() {
    ensure_crypto_provider();
    let mut params = CertificateParams::default();
    params.distinguished_name = DistinguishedName::new(); // no CN
    let key_pair = KeyPair::generate().unwrap();
    let cert = params.self_signed(&key_pair).unwrap();
    assert!(client_identity_from_der(cert.der()).is_none());
}

#[test]
fn loads_a_pem_bundle_with_one_ca() {
    ensure_crypto_provider();
    let mut dn = DistinguishedName::new();
    dn.push(DnType::CommonName, "test-ca");
    let mut params = CertificateParams::default();
    params.distinguished_name = dn;
    let key_pair = KeyPair::generate().unwrap();
    let cert = params.self_signed(&key_pair).unwrap();

    let dir = std::env::temp_dir().join(format!("nodelet-test-client-ca-{}", std::process::id()));
    std::fs::write(&dir, cert.pem()).unwrap();

    let store = load_client_ca(dir.to_str().unwrap()).expect("a single valid PEM CA cert must load");
    assert_eq!(store.len(), 1);
    std::fs::remove_file(&dir).unwrap();
}

#[test]
fn rejects_a_bundle_with_no_certificates() {
    let dir = std::env::temp_dir().join(format!("nodelet-test-client-ca-empty-{}", std::process::id()));
    std::fs::write(&dir, b"not a pem file").unwrap();
    assert!(load_client_ca(dir.to_str().unwrap()).is_err());
    std::fs::remove_file(&dir).unwrap();
}

#[test]
fn rejects_a_missing_file() {
    assert!(load_client_ca("/nonexistent/path/does-not-exist.pem").is_err());
}
