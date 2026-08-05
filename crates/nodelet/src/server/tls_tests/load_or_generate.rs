//! Real filesystem + crypto — no mocking, this is exactly what runs at
//! startup. The one thing this can't prove without a live cluster is that
//! a real TLS client (kubectl/apiserver) successfully completes a
//! handshake against the resulting rustls ServerConfig.
use super::*;

/// main.rs installs rustls's default CryptoProvider once at process
/// startup (needed before any TLS type is built); these tests exercise
/// that same path standalone, so they need to do it themselves exactly
/// once too — a second install_default() call errors.
fn ensure_crypto_provider() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

fn tmp_dir(name: &str) -> String {
    ensure_crypto_provider();
    let dir = std::env::temp_dir().join(format!("nodelet-test-tls-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir.to_string_lossy().into_owned()
}

#[test]
fn generates_a_cert_and_key_on_first_call() {
    let dir = tmp_dir("generate");
    let cert = load_or_generate(&dir, "test-node", "10.1.2.3").expect("cert generation should succeed");
    // A working rustls ServerConfig is the real proof — building one from
    // an invalid cert/key would panic/error inside server_config().
    let _config = cert.server_config(None).unwrap();
    assert!(std::path::Path::new(&dir).join("server.crt.der").exists());
    assert!(std::path::Path::new(&dir).join("server.key.der").exists());
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn cert_includes_the_node_ip_as_a_san() {
    // Confirmed for real against a live k3s apiserver: without the node's
    // own IP as a SAN, the apiserver's proxy dial to nodelet's server
    // (identified by Node.status.addresses' InternalIP, not hostname)
    // fails TLS verification outright — "doesn't contain any IP SANs" —
    // and exec/logs/attach/port-forward are all unreachable through it
    // despite the server itself working fine.
    let dir = tmp_dir("ip-san");
    let cert = load_or_generate(&dir, "test-node", "10.1.2.3").unwrap();
    let (_, parsed) = x509_parser::parse_x509_certificate(cert.cert_der.as_ref()).unwrap();
    let san_ext = parsed.subject_alternative_name().unwrap().expect("cert should have a SAN extension");
    let has_ip = san_ext.value.general_names.iter().any(|n| matches!(n, x509_parser::extensions::GeneralName::IPAddress(ip) if *ip == [10, 1, 2, 3]));
    assert!(has_ip, "cert's SAN list should include the node's IP (10.1.2.3) as an IP address entry");
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn reuses_an_existing_cert_on_second_call() {
    let dir = tmp_dir("reuse");
    load_or_generate(&dir, "test-node", "10.1.2.3").unwrap();
    let cert_bytes_first = std::fs::read(std::path::Path::new(&dir).join("server.crt.der")).unwrap();

    load_or_generate(&dir, "test-node", "10.1.2.3").unwrap();
    let cert_bytes_second = std::fs::read(std::path::Path::new(&dir).join("server.crt.der")).unwrap();

    assert_eq!(cert_bytes_first, cert_bytes_second, "a second call must not regenerate (would invalidate anything that already trusts/pinned the cert)");
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn regenerates_if_the_key_file_is_empty_or_missing() {
    let dir = tmp_dir("corrupt");
    load_or_generate(&dir, "test-node", "10.1.2.3").unwrap();
    std::fs::write(std::path::Path::new(&dir).join("server.key.der"), []).unwrap(); // corrupt it
    let cert = load_or_generate(&dir, "test-node", "10.1.2.3").expect("must regenerate instead of failing on a corrupt key file");
    let _config = cert.server_config(None).unwrap();
    std::fs::remove_dir_all(&dir).unwrap();
}

#[cfg(unix)]
#[test]
fn key_file_is_written_with_restrictive_permissions() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tmp_dir("perms");
    load_or_generate(&dir, "test-node", "10.1.2.3").unwrap();
    let mode = std::fs::metadata(std::path::Path::new(&dir).join("server.key.der")).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o600, "private key file should not be group/world readable");
    std::fs::remove_dir_all(&dir).unwrap();
}
