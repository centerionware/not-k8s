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
    let ca_key_path = dir.join("ca.key");
    let material = Material {
        ca: dir.join("ca.crt"),
        cert: dir.join("server.crt"),
        key: dir.join("server.key"),
        client_cert: (domain == Domain::Client).then(|| dir.join("client.crt")),
        client_key: (domain == Domain::Client).then(|| dir.join("client.key")),
    };

    // Every file this domain is supposed to end up with, so a *partially*
    // written set is repaired rather than mistaken for a complete one. The
    // client leaf in particular used not to be checked: a directory holding
    // ca.crt/server.crt/server.key but no client.crt returned here as if it
    // were finished, and deploy/bootstrap-source.sh then failed the whole run
    // with "nodestore is listening but did not write ..." on every subsequent
    // start, with no way to recover short of deleting the directory.
    let expected: Vec<&PathBuf> = [Some(&material.ca), Some(&material.cert), Some(&material.key)]
        .into_iter()
        .chain([material.client_cert.as_ref(), material.client_key.as_ref()])
        .flatten()
        .collect();
    if expected.iter().all(|p| p.exists()) {
        return Ok(material);
    }

    std::fs::create_dir_all(&dir)
        .map_err(|e| Error::Unavailable(format!("creating {}: {e}", dir.display())))?;

    // Regenerating a CA that peers already trust would partition the cluster,
    // so an existing one is reloaded and reused to sign whatever is missing.
    // This is why the CA *key* is persisted alongside ca.crt: without it a leaf
    // could never be reissued when it expires, and the only recovery would be
    // deleting the whole directory — which mints a new CA and breaks every peer
    // and client that trusted the old one.
    let (ca_cert, ca_key) = if material.ca.exists() {
        info!(?domain, dir = %dir.display(), "reusing the existing CA to issue missing TLS material");
        load_ca(&material.ca, &ca_key_path)?
    } else {
        info!(?domain, dir = %dir.display(), "generating TLS material (none configured)");
        let (ca_cert, ca_key) = generate_ca(domain)?;
        write_public(&material.ca, ca_cert.pem().as_bytes())?;
        write_secret(&ca_key_path, ca_key.serialize_pem().as_bytes())?;
        (ca_cert, ca_key)
    };

    if !material.cert.exists() || !material.key.exists() {
        let (cert_pem, key_pem) = sign_leaf(&ca_cert, &ca_key, sans, "nodestore-server")?;
        write_public(&material.cert, cert_pem.as_bytes())?;
        write_secret(&material.key, key_pem.as_bytes())?;
    }

    if let (Some(cc), Some(ck)) = (&material.client_cert, &material.client_key) {
        if !cc.exists() || !ck.exists() {
            // The client certificate needs no SANs: it is only ever verified as
            // a client, where the CN is the identity and SANs are irrelevant.
            let (c_pem, k_pem) = sign_leaf(&ca_cert, &ca_key, &[], "kube-apiserver")?;
            write_public(cc, c_pem.as_bytes())?;
            write_secret(ck, k_pem.as_bytes())?;
        }
    }

    Ok(material)
}

/// Reload a previously generated CA so it can sign further leaves.
///
/// Deliberately an error, not a silent regeneration, when the key is missing:
/// minting a fresh CA under the same path would leave every peer and client
/// still trusting the old one, and the resulting handshake failures read as a
/// network fault rather than a configuration one. Telling the operator to
/// remove the directory makes that a decision instead of an accident.
fn load_ca(ca_path: &Path, ca_key_path: &Path) -> Result<(rcgen::Certificate, KeyPair)> {
    if !ca_key_path.exists() {
        return Err(Error::Unavailable(format!(
            "{} exists but its private key {} does not, so no further certificate can be issued \
             from it. Remove {} to start over with a freshly generated CA — every peer and client \
             that trusted the old one will have to be given the new ca.crt.",
            ca_path.display(),
            ca_key_path.display(),
            ca_path.parent().unwrap_or(ca_path).display(),
        )));
    }
    let key_pem = String::from_utf8(read(ca_key_path)?)
        .map_err(|e| Error::Unavailable(format!("{} is not valid UTF-8 PEM: {e}", ca_key_path.display())))?;
    let ca_pem = String::from_utf8(read(ca_path)?)
        .map_err(|e| Error::Unavailable(format!("{} is not valid UTF-8 PEM: {e}", ca_path.display())))?;
    let key = KeyPair::from_pem(&key_pem)
        .map_err(|e| Error::Unavailable(format!("reading the CA key {}: {e}", ca_key_path.display())))?;
    // rcgen has no "certificate from PEM" type usable as an issuer, so the
    // params are read back out of the stored certificate and re-signed with the
    // stored key. Same subject, same key, so leaves signed by this chain verify
    // against the ca.crt already on disk and already distributed.
    let params = CertificateParams::from_ca_cert_pem(&ca_pem)
        .map_err(|e| Error::Unavailable(format!("reading the CA certificate {}: {e}", ca_path.display())))?;
    let cert = params
        .self_signed(&key)
        .map_err(|e| Error::Unavailable(format!("reconstructing the CA from {}: {e}", ca_path.display())))?;
    Ok((cert, key))
}

fn generate_ca(domain: Domain) -> Result<(rcgen::Certificate, KeyPair)> {
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
    Ok((cert, key))
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

    /// The CA key has to outlive the first start, or no leaf can ever be
    /// reissued and the only recovery is deleting the CA every peer trusts.
    #[test]
    fn the_ca_key_is_persisted_alongside_the_ca_certificate() {
        let dir = tempfile::tempdir().unwrap();
        let m = load_or_generate(dir.path(), Domain::Client, None, &["localhost".to_string()], false)
            .unwrap();
        let ca_key = m.ca.parent().unwrap().join("ca.key");
        assert!(ca_key.exists(), "the CA key must be kept, or leaves can never be reissued");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&ca_key).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "CA key must be 0600, got {mode:o}");
        }
    }

    /// A half-written directory used to read as a finished one: the reuse
    /// check looked only at ca/server, so a missing client leaf was never
    /// re-issued and bootstrap failed identically on every later start.
    #[test]
    fn a_missing_client_leaf_is_reissued_from_the_existing_ca() {
        let dir = tempfile::tempdir().unwrap();
        let sans = vec!["localhost".to_string()];
        let first = load_or_generate(dir.path(), Domain::Client, None, &sans, false).unwrap();
        let ca_before = std::fs::read(&first.ca).unwrap();
        let server_before = std::fs::read(&first.cert).unwrap();

        std::fs::remove_file(first.client_cert.as_ref().unwrap()).unwrap();
        std::fs::remove_file(first.client_key.as_ref().unwrap()).unwrap();

        let second = load_or_generate(dir.path(), Domain::Client, None, &sans, false).unwrap();
        assert!(second.client_cert.as_ref().unwrap().exists(), "the client leaf must be re-issued");
        assert!(second.client_key.as_ref().unwrap().exists());
        // ...from the *same* CA, and without disturbing the server leaf, or
        // every peer and client that trusted the old CA would break.
        assert_eq!(ca_before, std::fs::read(&second.ca).unwrap(), "the CA must not be regenerated");
        assert_eq!(server_before, std::fs::read(&second.cert).unwrap(), "the server leaf must be left alone");
    }

    /// Deployments created before the CA key was persisted have a ca.crt with
    /// no key. Silently minting a new CA under the same path would leave every
    /// peer trusting the old one, and the handshake failures would read as a
    /// network fault — so this says what happened and what it costs to fix.
    #[test]
    fn a_ca_with_no_key_is_a_clear_error_not_a_silent_replacement() {
        let dir = tempfile::tempdir().unwrap();
        let sans = vec!["localhost".to_string()];
        let m = load_or_generate(dir.path(), Domain::Client, None, &sans, false).unwrap();
        let ca_before = std::fs::read(&m.ca).unwrap();
        std::fs::remove_file(m.ca.parent().unwrap().join("ca.key")).unwrap();
        std::fs::remove_file(m.client_cert.as_ref().unwrap()).unwrap();

        let err = load_or_generate(dir.path(), Domain::Client, None, &sans, false)
            .expect_err("must not quietly replace a CA that peers already trust");
        let msg = err.to_string();
        assert!(msg.contains("ca.key"), "should name the missing key: {msg}");
        assert!(msg.contains("Remove"), "should say how to recover: {msg}");
        assert_eq!(ca_before, std::fs::read(&m.ca).unwrap(), "the existing CA must be left untouched");
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
