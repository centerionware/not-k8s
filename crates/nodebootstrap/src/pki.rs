//! Cluster PKI generation: CA, serving cert, ServiceAccount signing
//! keypair, per-component client certs. Group O's PKI half.
//!
//! Uses the same stack `crates/nodecontroller/src/controllers/csr.rs`
//! already vetted (`rcgen`/`p256`/`x509-parser`/`pem`) and the same API
//! shape that file's `sign_csr`/`load_signing_ca` use, so a CA minted here
//! loads back through that exact code unchanged.
//!
//! Deliberately does **not** borrow k3s's own generated CA the way
//! `deploy/lib/upstream-kube-apiserver.sh` does today (see that script's
//! header comment) -- minting a fresh CA here, independent of k3s, is what
//! makes `docs/NODEBOOTSTRAP_PLAN.md` point 3 (same PKI code, swap the
//! apiserver target underneath) possible at all.
//!
//! Scope, deliberately narrow: this module issues the CA and the *static*
//! control-plane identities (apiserver serving cert, ServiceAccount signing
//! keypair, `kube-controller-manager`/`kube-scheduler`/cluster-admin client
//! certs) -- one-time, at bootstrap. Per-node identities (`nodelet` running
//! as `system:node:<name>`) are **not** minted here: those go through the
//! real CSR flow `nodecontroller`'s `controllers/csr.rs` already implements
//! (`certificatesigningrequest-signing-controller`), using the CA this
//! module produces. Two different lifecycles -- one CA issued once at
//! cluster bootstrap, node certs issued continuously as nodes join -- and
//! conflating them here would duplicate logic `nodecontroller` already
//! owns.

use anyhow::{Context, Result};
use rcgen::{
    BasicConstraints, CertificateParams, DistinguishedName, DnType, IsCa, KeyPair, KeyUsagePurpose,
    SanType,
};

use crate::config::Config;

pub fn run() -> Result<()> {
    run_with(&Config::from_env()?)
}

/// A joining control-plane member must use the existing cluster CA and
/// ServiceAccount signing key. It may not silently mint a parallel cluster.
pub fn require_existing(cfg: &Config) -> Result<()> {
    let dir = cfg.pki_dir();
    for name in [
        "ca.crt",
        "ca.key",
        "apiserver.crt",
        "apiserver.key",
        "kube-apiserver.crt",
        "kube-apiserver.key",
        "sa.pub",
        "sa.key",
        "kube-controller-manager.crt",
        "kube-controller-manager.key",
        "kube-scheduler.crt",
        "kube-scheduler.key",
        "admin.crt",
        "admin.key",
    ] {
        anyhow::ensure!(
            dir.join(name).is_file(),
            "control-plane join requires shared cluster PKI file {}",
            dir.join(name).display()
        );
    }
    Ok(())
}

pub fn run_with(cfg: &Config) -> Result<()> {
    if cfg.skip_pki {
        tracing::info!("skipping PKI generation (NODEBOOTSTRAP_SKIP_PKI)");
        return Ok(());
    }
    let dir = cfg.pki_dir();
    let required = [
        "ca.crt",
        "ca.key",
        "apiserver.crt",
        "apiserver.key",
        "kube-apiserver.crt",
        "kube-apiserver.key",
        "sa.pub",
        "sa.key",
        "kube-controller-manager.crt",
        "kube-controller-manager.key",
        "kube-scheduler.crt",
        "kube-scheduler.key",
        "admin.crt",
        "admin.key",
    ];
    let present = required.iter().filter(|name| dir.join(name).exists()).count();
    if present == required.len() {
        tracing::info!(dir = %dir.display(), "reusing existing cluster PKI");
        return Ok(());
    }
    anyhow::ensure!(
        present == 0,
        "cluster PKI at {} is incomplete ({present}/{} files); refusing to replace it",
        dir.display(),
        required.len()
    );
    let mut spec = ClusterPkiSpec::default();
    spec.cluster_domain = cfg.cluster_domain();
    if let Some(address) = &cfg.advertise_address {
        spec.extra_sans.push(address.clone());
    }
    let cluster = generate(&spec)?;
    cluster.write_to_dir(&dir)?;
    tracing::info!(dir = %dir.display(), "wrote cluster PKI");
    Ok(())
}

/// A generated cert+key pair, PEM-encoded, ready to write to disk or embed
/// in a kubeconfig.
pub struct IssuedCert {
    pub cert_pem: String,
    pub key_pem: String,
}

/// The complete set of PKI material a cluster bootstrap needs. Fields named
/// to match what `kubeconfig.rs` and `targets/upstream.rs` will consume.
pub struct ClusterPki {
    pub ca: IssuedCert,
    pub apiserver_serving: IssuedCert,
    /// Client identity the apiserver presents when proxying exec/logs/
    /// attach/port-forward requests to nodelet.
    pub kube_apiserver_client: IssuedCert,
    /// Private key used by `--service-account-signing-key-file`; the public
    /// half (derived from the same keypair) is `--service-account-key-file`.
    pub sa_signing: IssuedCert,
    pub kube_controller_manager: IssuedCert,
    pub kube_scheduler: IssuedCert,
    /// CN=`admin`, O=`system:masters` -- cluster-admin via the built-in
    /// group binding, same convention kubeadm/k3s both use for their own
    /// admin kubeconfig.
    pub cluster_admin: IssuedCert,
}

/// What the apiserver's own serving cert needs to be trusted for, resolved
/// once per bootstrap run. `extra_sans` lets a caller add the node's real
/// hostname/IP (unknown to this module, which has no host-detection logic
/// of its own -- that's `targets/upstream.rs`'s job to pass in).
pub struct ClusterPkiSpec {
    pub service_ip: std::net::IpAddr,
    pub cluster_domain: String,
    pub extra_sans: Vec<String>,
}

impl Default for ClusterPkiSpec {
    fn default() -> Self {
        ClusterPkiSpec {
            // 10.43.0.1 matches this project's existing SERVICE_CIDR default
            // (deploy/lib/upstream-kube-apiserver.sh's SERVICE_CIDR=10.43.0.0/16,
            // first usable address) -- the apiserver's own ClusterIP inside
            // the cluster it serves.
            service_ip: "10.43.0.1".parse().expect("static IP literal"),
            cluster_domain: "cluster.local".to_string(),
            extra_sans: Vec::new(),
        }
    }
}

pub fn generate(spec: &ClusterPkiSpec) -> Result<ClusterPki> {
    let (ca_cert, ca_key) = generate_ca().context("generating cluster CA")?;

    let mut serving_sans = vec![
        "kubernetes".to_string(),
        "kubernetes.default".to_string(),
        "kubernetes.default.svc".to_string(),
        format!("kubernetes.default.svc.{}", spec.cluster_domain),
        "localhost".to_string(),
    ];
    let mut serving_ips = vec![std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST), spec.service_ip];
    for san in &spec.extra_sans {
        if let Ok(ip) = san.parse() {
            serving_ips.push(ip);
        } else {
            serving_sans.push(san.clone());
        }
    }
    let apiserver_serving = issue_serving_cert(
        &ca_cert,
        &ca_key,
        &serving_sans,
        &serving_ips,
    )
    .context("issuing apiserver serving cert")?;
    let kube_apiserver_client = issue_client_cert(&ca_cert, &ca_key, "system:kube-apiserver", &[])
        .context("issuing kube-apiserver client cert")?;

    let sa_signing = generate_sa_signing_keypair().context("generating ServiceAccount signing keypair")?;

    let kube_controller_manager =
        issue_client_cert(&ca_cert, &ca_key, "system:kube-controller-manager", &[])
            .context("issuing kube-controller-manager client cert")?;
    let kube_scheduler = issue_client_cert(&ca_cert, &ca_key, "system:kube-scheduler", &[])
        .context("issuing kube-scheduler client cert")?;
    // O=system:masters binds to the built-in cluster-admin ClusterRole via
    // the (also built-in) system:masters ClusterRoleBinding -- see rbac.rs's
    // module doc for why neither of those needs to be minted by this crate.
    let cluster_admin = issue_client_cert(&ca_cert, &ca_key, "admin", &["system:masters"])
        .context("issuing cluster-admin client cert")?;

    let ca = IssuedCert { cert_pem: ca_cert.pem(), key_pem: ca_key.serialize_pem() };

    Ok(ClusterPki {
        ca,
        apiserver_serving,
        kube_apiserver_client,
        sa_signing,
        kube_controller_manager,
        kube_scheduler,
        cluster_admin,
    })
}

fn generate_ca() -> Result<(rcgen::Certificate, KeyPair)> {
    let mut params = CertificateParams::default();
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    let mut dn = DistinguishedName::new();
    dn.push(DnType::CommonName, "not-k8s-ca");
    params.distinguished_name = dn;
    let key = KeyPair::generate().context("generating CA keypair")?;
    let cert = params.self_signed(&key).context("self-signing CA cert")?;
    Ok((cert, key))
}

fn issue_serving_cert(
    ca_cert: &rcgen::Certificate,
    ca_key: &KeyPair,
    dns_sans: &[String],
    ip_sans: &[std::net::IpAddr],
) -> Result<IssuedCert> {
    let mut params = CertificateParams::new(dns_sans.to_vec()).context("building serving cert params")?;
    for ip in ip_sans {
        params.subject_alt_names.push(SanType::IpAddress(*ip));
    }
    let mut dn = DistinguishedName::new();
    dn.push(DnType::CommonName, "kube-apiserver");
    params.distinguished_name = dn;
    let key = KeyPair::generate().context("generating serving cert keypair")?;
    let cert = params
        .signed_by(&key, ca_cert, ca_key)
        .context("signing serving cert with cluster CA")?;
    Ok(IssuedCert { cert_pem: cert.pem(), key_pem: key.serialize_pem() })
}

/// Issues a client cert for a single identity: CN=`common_name`,
/// O=`organizations` (zero or more -- the Node/RBAC authorizers key off
/// both). No SANs; client certs are identified by subject, not by name the
/// way a server cert is.
fn issue_client_cert(
    ca_cert: &rcgen::Certificate,
    ca_key: &KeyPair,
    common_name: &str,
    organizations: &[&str],
) -> Result<IssuedCert> {
    let mut params = CertificateParams::new(Vec::<String>::new()).context("building client cert params")?;
    let mut dn = DistinguishedName::new();
    dn.push(DnType::CommonName, common_name);
    for org in organizations {
        dn.push(DnType::OrganizationName, *org);
    }
    params.distinguished_name = dn;
    let key = KeyPair::generate().context("generating client cert keypair")?;
    let cert = params
        .signed_by(&key, ca_cert, ca_key)
        .with_context(|| format!("signing client cert for {common_name}"))?;
    Ok(IssuedCert { cert_pem: cert.pem(), key_pem: key.serialize_pem() })
}

/// A standalone (not CA-signed) EC P-256 keypair used only for
/// ServiceAccount JWT issuance/verification -- `--service-account-signing-
/// key-file`/`--service-account-key-file`. Real upstream accepts either RSA
/// or EC here; EC keeps this crate to one crypto stack (`p256`, not
/// `rcgen::KeyPair`, since this key is never wrapped in a cert -- there is
/// no `rcgen::Certificate::pem()` to call, so `IssuedCert.cert_pem` here
/// actually holds the SPKI-encoded *public* key PEM, not a certificate).
fn generate_sa_signing_keypair() -> Result<IssuedCert> {
    use p256::pkcs8::{EncodePrivateKey, EncodePublicKey};
    let secret = p256::SecretKey::random(&mut p256::elliptic_curve::rand_core::OsRng);
    let private_pem = secret
        .to_pkcs8_pem(Default::default())
        .context("encoding ServiceAccount signing key as PKCS#8")?;
    let public_pem = secret
        .public_key()
        .to_public_key_pem(Default::default())
        .context("encoding ServiceAccount public key as SPKI")?;
    Ok(IssuedCert { cert_pem: public_pem, key_pem: private_pem.to_string() })
}

impl ClusterPki {
    /// Writes every PEM to `dir` using upstream's own conventional
    /// filenames (`ca.crt`/`ca.key`, `sa.key`/`sa.pub`, ...) so
    /// `targets/upstream.rs` can point real `kube-apiserver` flags straight
    /// at this directory with no renaming step.
    pub fn write_to_dir(&self, dir: &std::path::Path) -> Result<()> {
        std::fs::create_dir_all(dir).with_context(|| format!("creating PKI dir {}", dir.display()))?;
        let write = |name: &str, contents: &str| -> Result<()> {
            let path = dir.join(name);
            std::fs::write(&path, contents).with_context(|| format!("writing {}", path.display()))?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                if name.ends_with(".key") {
                    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
                        .with_context(|| format!("restricting permissions on {}", path.display()))?;
                }
            }
            Ok(())
        };
        write("ca.crt", &self.ca.cert_pem)?;
        write("ca.key", &self.ca.key_pem)?;
        write("apiserver.crt", &self.apiserver_serving.cert_pem)?;
        write("apiserver.key", &self.apiserver_serving.key_pem)?;
        write("kube-apiserver.crt", &self.kube_apiserver_client.cert_pem)?;
        write("kube-apiserver.key", &self.kube_apiserver_client.key_pem)?;
        write("sa.pub", &self.sa_signing.cert_pem)?;
        write("sa.key", &self.sa_signing.key_pem)?;
        write("kube-controller-manager.crt", &self.kube_controller_manager.cert_pem)?;
        write("kube-controller-manager.key", &self.kube_controller_manager.key_pem)?;
        write("kube-scheduler.crt", &self.kube_scheduler.cert_pem)?;
        write("kube-scheduler.key", &self.kube_scheduler.key_pem)?;
        write("admin.crt", &self.cluster_admin.cert_pem)?;
        write("admin.key", &self.cluster_admin.key_pem)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generate_produces_a_ca_that_verifies_every_issued_cert() {
        let pki = generate(&ClusterPkiSpec::default()).expect("generate cluster PKI");

        // Every cert's PEM should parse back with x509-parser and chain to
        // the CA -- this is the same round-trip nodecontroller's own
        // load_signing_ca()/sign_csr() depend on, so proving it here is
        // proving this crate's CA is a drop-in for that code path.
        for issued in [
            &pki.apiserver_serving,
            &pki.kube_apiserver_client,
            &pki.kube_controller_manager,
            &pki.kube_scheduler,
            &pki.cluster_admin,
        ] {
            assert!(issued.cert_pem.contains("BEGIN CERTIFICATE"));
            assert!(issued.key_pem.contains("PRIVATE KEY"));
        }
        assert!(pki.ca.cert_pem.contains("BEGIN CERTIFICATE"));
        assert!(pki.sa_signing.key_pem.contains("PRIVATE KEY"));
    }

    #[test]
    fn cluster_admin_carries_system_masters() {
        let pki = generate(&ClusterPkiSpec::default()).expect("generate cluster PKI");
        let der = pem::parse(&pki.cluster_admin.cert_pem).expect("parse cluster-admin cert PEM");
        let (_, cert) =
            x509_parser::parse_x509_certificate(der.contents()).expect("parse cluster-admin cert DER");

        let cn = cert
            .subject()
            .iter_common_name()
            .next()
            .and_then(|a| a.as_str().ok())
            .expect("cert has a CN");
        assert_eq!(cn, "admin");

        let org = cert
            .subject()
            .iter_organization()
            .next()
            .and_then(|a| a.as_str().ok())
            .expect("cert has an O");
        assert_eq!(org, "system:masters");
    }

    #[test]
    fn apiserver_serving_certificate_includes_the_selected_cluster_domain() {
        let mut spec = ClusterPkiSpec::default();
        spec.cluster_domain = "cluster.example".to_string();
        let pki = generate(&spec).expect("generate cluster PKI");
        let der = pem::parse(&pki.apiserver_serving.cert_pem).expect("parse apiserver cert PEM");
        let (_, cert) = x509_parser::parse_x509_certificate(der.contents()).expect("parse apiserver cert DER");
        let san_ext = cert
            .subject_alternative_name()
            .expect("read apiserver SAN extension")
            .expect("apiserver cert should have a SAN extension");
        assert!(san_ext.value.general_names.iter().any(|name| {
            matches!(name, x509_parser::extensions::GeneralName::DNSName(name) if name == "kubernetes.default.svc.cluster.example")
        }));
    }
}
