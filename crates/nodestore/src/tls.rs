//! TLS for the datastore: both the client API and the raft peer link.
//!
//! # Why this is not optional
//!
//! The datastore holds every Secret, ServiceAccount token and object in the
//! cluster, and the etcd v3 API has no authentication of its own — anything
//! that can open a socket to it can read and write all of it. Serving that in
//! plaintext is strictly worse than an unauthenticated apiserver, because
//! there is not even an authorization layer above it to fail closed.
//!
//! So there is no "off". The only choice an operator has is whether to bring
//! their own PKI or let this generate one.
//!
//! # How Kubernetes does it, and what this mirrors
//!
//! etcd separates two trust domains, and so does this:
//!
//!   * **Client** — `--cert-file`/`--key-file`/`--trusted-ca-file`, with
//!     `--client-cert-auth` requiring clients to present a certificate. This
//!     is what kube-apiserver dials with `--etcd-certfile`/`--etcd-keyfile`/
//!     `--etcd-cafile` (k3s spells the same thing `--datastore-certfile`/
//!     `--datastore-keyfile`/`--datastore-cafile`).
//!   * **Peer** — a *separate* `--peer-cert-file`/`--peer-key-file`/
//!     `--peer-trusted-ca-file` set for the raft link between members.
//!
//! Keeping them separate matters: a client certificate is handed to
//! kube-apiserver, and possibly to operators for debugging. If that same
//! trust domain also admitted a holder as a *peer*, anyone with a client cert
//! could join the raft cluster and rewrite history. Two CAs means a leaked
//! client cert can read and write data — bad — but cannot become a member.
//!
//! # Generated for a single member, required for a cluster
//!
//! Requiring hand-built PKI before a single-node store will start is how
//! deployments end up disabling security instead of configuring it. So for a
//! single member, a CA per trust domain and the leaf certificates are
//! generated into `$NODESTORE_DATA_DIR/pki/` on first start and reused
//! afterwards.
//!
//! This is not "self-signed so it's insecure": the client CA is a real CA,
//! client certificates are verified against it, and a client without one
//! cannot complete a handshake.
//!
//! A **cluster** must be given its material. Generation cannot be extended
//! there, because each member would generate its own CA and no member would
//! trust any other — the cluster simply would not form, with a handshake
//! error that reads as a network fault. Distributing a CA before raft is up
//! needs a bootstrap channel that itself must be authenticated, which is the
//! chicken-and-egg etcd resolves by never generating PKI at all. A clustered
//! member with no configured material therefore refuses to start.

use crate::error::{Error, Result};
use rcgen::{
    BasicConstraints, CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa, KeyPair,
    KeyUsagePurpose,
};
use std::path::{Path, PathBuf};
use tracing::info;

/// The two trust domains. They never share a CA — see the module header.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Domain {
    /// kube-apiserver and anything else speaking the etcd v3 API.
    Client,
    /// The raft link between members.
    Peer,
}

impl Domain {
    fn dir_name(self) -> &'static str {
        match self {
            Domain::Client => "client",
            Domain::Peer => "peer",
        }
    }

    fn ca_common_name(self) -> &'static str {
        match self {
            Domain::Client => "nodestore-client-ca",
            Domain::Peer => "nodestore-peer-ca",
        }
    }
}

/// Paths to one trust domain's material. All PEM, because that is what
/// kube-apiserver, k3s and etcdctl all expect to be handed — unlike nodelet's
/// server cert, which is only ever read back by nodelet itself and so can
/// stay DER.
#[derive(Clone, Debug)]
pub struct Material {
    /// CA bundle to verify the other end against.
    pub ca: PathBuf,
    /// This member's own certificate.
    pub cert: PathBuf,
    /// Its key.
    pub key: PathBuf,
    /// A client certificate signed by the client CA, for handing to
    /// kube-apiserver. Only produced for [`Domain::Client`].
    pub client_cert: Option<PathBuf>,
    pub client_key: Option<PathBuf>,
}

/// Load the configured material, or generate it if nothing is configured.
///
/// `sans` are the names and addresses this member is reachable as. A missing
/// SAN is the single most common way a working PKI still fails to connect, so
/// callers pass everything: the advertise URL's host, the listen address, the
/// hostname, and loopback.
pub fn load_or_generate(
    data_dir: &Path,
    domain: Domain,
    configured: Option<Material>,
    sans: &[String],
    clustered: bool,
) -> Result<Material> {
    // Generation is a single-member convenience and cannot be extended to a
    // cluster: each member would generate its *own* CA, so no member would
    // trust any other's certificate and no client certificate would be
    // accepted by more than one of them. The cluster would fail to form, with
    // a handshake error that looks like a network problem.
    //
    // Distributing a CA between members before raft is up would need a
    // bootstrap channel that itself has to be authenticated — the same
    // chicken-and-egg etcd resolves by simply not generating PKI at all and
    // requiring the operator to supply it. (k3s generates and distributes one
    // only because it owns the whole cluster lifecycle, including a join
    // token; nodestore has no such channel.)
    //
    // So a clustered member without configured material refuses to start,
    // rather than starting and failing later in a way that reads as a
    // networking fault.
    if clustered && configured.is_none() {
        return Err(Error::InvalidRequest(format!(
            "a clustered member must be given TLS material: set NODESTORE{}_CERT_FILE, \
             NODESTORE{}_KEY_FILE and NODESTORE{}_TRUSTED_CA_FILE. Certificates are only \
             generated for a single-member store, because every member of a cluster has to share \
             one CA — if each generated its own, no member would trust any other and the cluster \
             could not form.",
            match domain {
                Domain::Client => "",
                Domain::Peer => "_PEER",
            },
            match domain {
                Domain::Client => "",
                Domain::Peer => "_PEER",
            },
            match domain {
                Domain::Client => "",
                Domain::Peer => "_PEER",
            },
        )));
    }

    if let Some(m) = configured {
        // Fail loudly rather than silently falling back to generated certs:
        // an operator who configured PKI and got self-signed material anyway
        // would have no way to notice.
        for (what, path) in [("certificate", &m.cert), ("key", &m.key), ("CA", &m.ca)] {
            if !path.exists() {
                return Err(Error::InvalidRequest(format!(
                    "configured {domain:?} {what} file does not exist: {}",
                    path.display()
                )));
            }
        }
        info!(?domain, ca = %m.ca.display(), "using configured TLS material");
        return Ok(m);
    }

    let dir = data_dir.join("pki").join(domain.dir_name());
    let material = Material {
        ca: dir.join("ca.crt"),
        cert: dir.join("server.crt"),
        key: dir.join("server.key"),
        client_cert: (domain == Domain::Client).then(|| dir.join("client.crt")),
        client_key: (domain == Domain::Client).then(|| dir.join("client.key")),
    };

    // Regenerating a CA that peers already trust would partition the cluster,
    // so existing material is always reused.
    if material.ca.exists() && material.cert.exists() && material.key.exists() {
        return Ok(material);
    }

    std::fs::create_dir_all(&dir)
        .map_err(|e| Error::Unavailable(format!("creating {}: {e}", dir.display())))?;

    info!(?domain, dir = %dir.display(), "generating TLS material (none configured)");

    let (ca_pem, ca_cert_params_key) = generate_ca(domain)?;
    write_secret(&material.ca, ca_pem.as_bytes())?;

    let (ca_cert, ca_key) = ca_cert_params_key;

    let (cert_pem, key_pem) = sign_leaf(&ca_cert, &ca_key, sans, "nodestore-server")?;
    write_public(&material.cert, cert_pem.as_bytes())?;
    write_secret(&material.key, key_pem.as_bytes())?;

    if let (Some(cc), Some(ck)) = (&material.client_cert, &material.client_key) {
        // The client certificate needs no SANs: it is only ever verified as a
        // client, where the CN is the identity and SANs are irrelevant.
        let (c_pem, k_pem) = sign_leaf(&ca_cert, &ca_key, &[], "kube-apiserver")?;
        write_public(cc, c_pem.as_bytes())?;
        write_secret(ck, k_pem.as_bytes())?;
    }

    Ok(material)
}

fn generate_ca(domain: Domain) -> Result<(String, (rcgen::Certificate, KeyPair))> {
    let mut params = CertificateParams::new(Vec::<String>::new())
        .map_err(|e| Error::Unavailable(format!("building CA parameters: {e}")))?;
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    params.distinguished_name.push(DnType::CommonName, domain.ca_common_name());
    params.key_usages =
        vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign, KeyUsagePurpose::DigitalSignature];

    let key = KeyPair::generate().map_err(|e| Error::Unavailable(format!("generating CA key: {e}")))?;
    let cert = params
        .self_signed(&key)
        .map_err(|e| Error::Unavailable(format!("self-signing the CA: {e}")))?;
    Ok((cert.pem(), (cert, key)))
}

fn sign_leaf(
    ca_cert: &rcgen::Certificate,
    ca_key: &KeyPair,
    sans: &[String],
    common_name: &str,
) -> Result<(String, String)> {
    let mut params = CertificateParams::new(sans.to_vec())
        .map_err(|e| Error::Unavailable(format!("building certificate parameters: {e}")))?;
    params.distinguished_name.push(DnType::CommonName, common_name);
    params.use_authority_key_identifier_extension = true;
    params.key_usages =
        vec![KeyUsagePurpose::DigitalSignature, KeyUsagePurpose::KeyEncipherment];
    // Both purposes on every leaf: a member is a server to its peers on one
    // connection and a client to them on another, and the forwarding path
    // makes a follower a client of the leader's *client* API.
    params.extended_key_usages = vec![
        ExtendedKeyUsagePurpose::ServerAuth,
        ExtendedKeyUsagePurpose::ClientAuth,
    ];

    let key = KeyPair::generate()
        .map_err(|e| Error::Unavailable(format!("generating leaf key: {e}")))?;
    let cert = params
        .signed_by(&key, ca_cert, ca_key)
        .map_err(|e| Error::Unavailable(format!("signing certificate: {e}")))?;
    Ok((cert.pem(), key.serialize_pem()))
}

/// Server-side TLS for one trust domain, with client certificates
/// **required**.
///
/// `client_ca_root` is what makes this mutual rather than merely encrypted:
/// tonic will refuse a handshake from a client that presents no certificate,
/// or one that does not chain to this CA, before any of our code runs. That
/// is the authentication — the etcd v3 API itself has none, so if this were
/// server-only TLS, anything that could reach the port would still have full
/// read/write access over a nicely encrypted channel.
pub fn server_tls_config(m: &Material) -> Result<tonic::transport::ServerTlsConfig> {
    let cert = read(&m.cert)?;
    let key = read(&m.key)?;
    let ca = read(&m.ca)?;
    Ok(tonic::transport::ServerTlsConfig::new()
        .identity(tonic::transport::Identity::from_pem(cert, key))
        .client_ca_root(tonic::transport::Certificate::from_pem(ca)))
}

/// Client-side TLS for one trust domain, presenting this member's own
/// certificate.
///
/// Used for both directions a member dials: raft messages to a peer, and a
/// follower forwarding a write to the leader's client API. Both ends verify
/// each other — the identity here is what the server's `client_ca_root`
/// above checks.
///
/// `domain_name` overrides the name verified against the server's
/// certificate. Needed because members address each other by URL, and a URL
/// built from an IP has to match an IP SAN.
pub fn client_tls_config(
    m: &Material,
    domain_name: Option<&str>,
) -> Result<tonic::transport::ClientTlsConfig> {
    let cert = read(&m.cert)?;
    let key = read(&m.key)?;
    let ca = read(&m.ca)?;
    let mut cfg = tonic::transport::ClientTlsConfig::new()
        .ca_certificate(tonic::transport::Certificate::from_pem(ca))
        .identity(tonic::transport::Identity::from_pem(cert, key));
    if let Some(name) = domain_name {
        cfg = cfg.domain_name(name.to_string());
    }
    Ok(cfg)
}

fn read(path: &Path) -> Result<Vec<u8>> {
    std::fs::read(path).map_err(|e| Error::Unavailable(format!("reading {}: {e}", path.display())))
}

fn write_public(path: &Path, bytes: &[u8]) -> Result<()> {
    std::fs::write(path, bytes)
        .map_err(|e| Error::Unavailable(format!("writing {}: {e}", path.display())))
}

/// Keys and CA material are written 0600. A world-readable private key in the
/// data directory would undo the entire point of this module.
fn write_secret(path: &Path, bytes: &[u8]) -> Result<()> {
    write_public(path, bytes)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .map_err(|e| Error::Unavailable(format!("chmod {}: {e}", path.display())))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_material_lands_on_disk_and_is_reused() {
        let dir = tempfile::tempdir().unwrap();
        let sans = vec!["localhost".to_string(), "127.0.0.1".to_string()];

        let first = load_or_generate(dir.path(), Domain::Client, None, &sans, false).unwrap();
        assert!(first.ca.exists() && first.cert.exists() && first.key.exists());
        // A client certificate is produced for the client domain so
        // kube-apiserver has something to present.
        assert!(first.client_cert.as_ref().unwrap().exists());

        let ca_before = std::fs::read(&first.ca).unwrap();
        let second = load_or_generate(dir.path(), Domain::Client, None, &sans, false).unwrap();
        // Regenerating a CA that peers already trust would partition the
        // cluster, so a second start must reuse it byte for byte.
        assert_eq!(ca_before, std::fs::read(&second.ca).unwrap());
    }

    #[test]
    fn the_peer_domain_gets_its_own_ca_and_no_client_cert() {
        let dir = tempfile::tempdir().unwrap();
        let sans = vec!["localhost".to_string()];

        let client = load_or_generate(dir.path(), Domain::Client, None, &sans, false).unwrap();
        let peer = load_or_generate(dir.path(), Domain::Peer, None, &sans, false).unwrap();

        // The whole point of two trust domains: a client certificate must not
        // be able to join the raft cluster.
        assert_ne!(
            std::fs::read(&client.ca).unwrap(),
            std::fs::read(&peer.ca).unwrap(),
            "client and peer CAs must be distinct, or a leaked client cert becomes a member"
        );
        assert!(peer.client_cert.is_none());
    }

    #[test]
    fn a_configured_path_that_does_not_exist_is_an_error_not_a_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let configured = Material {
            ca: dir.path().join("nope-ca.crt"),
            cert: dir.path().join("nope.crt"),
            key: dir.path().join("nope.key"),
            client_cert: None,
            client_key: None,
        };
        let err = load_or_generate(dir.path(), Domain::Client, Some(configured), &[], false)
            .expect_err("must not silently generate its own material instead");
        assert!(err.to_string().contains("does not exist"), "got: {err}");
    }

    // A cluster that generated per-member CAs would fail to form, and the
    // handshake error would look like a network problem rather than a
    // configuration one. Refusing at startup is the whole point.
    #[test]
    fn a_clustered_member_refuses_to_generate_its_own_ca() {
        let dir = tempfile::tempdir().unwrap();
        let err = load_or_generate(dir.path(), Domain::Client, None, &[], true)
            .expect_err("a clustered member must not generate a CA only it trusts");
        let msg = err.to_string();
        assert!(msg.contains("NODESTORE_CERT_FILE"), "should name the variable to set: {msg}");
        assert!(msg.contains("share"), "should explain why: {msg}");

        // ...and the peer domain names its own variables, not the client ones.
        let err = load_or_generate(dir.path(), Domain::Peer, None, &[], true).unwrap_err();
        assert!(
            err.to_string().contains("NODESTORE_PEER_CERT_FILE"),
            "peer domain should name the peer variables: {err}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn private_keys_are_not_world_readable() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let m = load_or_generate(dir.path(), Domain::Client, None, &["localhost".to_string()], false)
            .unwrap();
        let mode = std::fs::metadata(&m.key).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "server key must be 0600, got {mode:o}");
        let cmode =
            std::fs::metadata(m.client_key.as_ref().unwrap()).unwrap().permissions().mode() & 0o777;
        assert_eq!(cmode, 0o600, "client key must be 0600, got {cmode:o}");
    }
}
