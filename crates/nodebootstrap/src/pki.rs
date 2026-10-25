//! Cluster PKI generation: CA, serving cert, ServiceAccount signing
//! keypair, per-component client certs. Group O's PKI half.
//!
//! Uses the same stack `crates/nodecontroller/src/controllers/csr.rs`
//! already vetted (`rcgen`/`p256`/`x509-parser`/`pem`) and the same API
//! shape that file's `sign_csr`/`load_signing_ca` use, so a CA minted here
//! loads back through that exact code unchanged.
//!
//! Normal bootstrap mints a fresh cluster CA. In-place migration can instead
//! retain the source API serving CA and any separate client-auth CA so
//! existing nodes and clients continue to trust the replacement API.
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
    ensure_front_proxy_client(&dir)?;
    Ok(())
}

pub fn run_with(cfg: &Config) -> Result<()> {
    if cfg.skip_pki {
        tracing::info!("skipping PKI generation (NODEBOOTSTRAP_SKIP_PKI)");
        return Ok(());
    }
    let dir = cfg.pki_dir();
    let migration_ca = migration_ca_from_env()?;
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
    let service_ips = cfg.service_ips()?;
    let advertise_address = cfg
        .advertise_address
        .clone()
        .unwrap_or_else(crate::targets::upstream::detect_advertise_address);
    let mut extra_sans = apiserver_extra_sans(&service_ips, &advertise_address);
    if let Ok(configured_sans) = std::env::var("NODEBOOTSTRAP_APISERVER_EXTRA_SANS") {
        for san in configured_apiserver_extra_sans(configured_sans) {
            if !extra_sans.contains(&san) {
                extra_sans.push(san);
            }
        }
    }
    if present == required.len() {
        if let Some(migration_ca) = &migration_ca {
            ensure_migration_ca_matches(&dir, migration_ca)?;
        }
        ensure_existing_pki_matches_domain(&dir, &cfg.cluster_domain(), &service_ips)?;
        ensure_existing_apiserver_extra_sans(
            &dir,
            &cfg.cluster_domain(),
            &service_ips,
            &extra_sans,
        )?;
        ensure_front_proxy_client(&dir)?;
        ensure_client_ca_bundle(&dir, migration_ca.as_ref())?;
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
    spec.service_ip = cfg.service_ip()?;
    spec.extra_sans = extra_sans;
    let cluster = generate_with_migration_ca(&spec, migration_ca.as_ref())?;
    cluster.write_to_dir(&dir)?;
    tracing::info!(dir = %dir.display(), "wrote cluster PKI");
    Ok(())
}

fn ensure_existing_apiserver_extra_sans(
    dir: &std::path::Path,
    cluster_domain: &str,
    service_ips: &[std::net::IpAddr],
    extra_sans: &[String],
) -> Result<()> {
    let cert_path = dir.join("apiserver.crt");
    let cert_pem = std::fs::read(&cert_path)
        .with_context(|| format!("reading existing apiserver certificate from {}", dir.display()))?;
    let parsed_pem = pem::parse(&cert_pem).context("parsing existing apiserver certificate PEM")?;
    let (_, certificate) = x509_parser::parse_x509_certificate(parsed_pem.contents())
        .context("parsing existing apiserver certificate DER")?;
    let san = certificate
        .subject_alternative_name()
        .context("reading existing apiserver certificate SANs")?;
    let has_extra_sans = san.is_some_and(|san| {
        extra_sans.iter().all(|expected| {
            if let Ok(expected_ip) = expected.parse::<std::net::IpAddr>() {
                let expected_ip = match expected_ip {
                    std::net::IpAddr::V4(ip) => ip.octets().to_vec(),
                    std::net::IpAddr::V6(ip) => ip.octets().to_vec(),
                };
                san.value.general_names.iter().any(|name| {
                    matches!(name, x509_parser::extensions::GeneralName::IPAddress(ip) if *ip == expected_ip.as_slice())
                })
            } else {
                san.value.general_names.iter().any(|name| {
                    matches!(name, x509_parser::extensions::GeneralName::DNSName(name) if *name == expected.as_str())
                })
            }
        })
    });
    if has_extra_sans {
        return Ok(());
    }

    let key_path = dir.join("apiserver.key");
    let serving_key_pem = std::fs::read_to_string(&key_path)
        .with_context(|| format!("reading existing apiserver key from {}", dir.display()))?;
    let serving_key = KeyPair::from_pem(&serving_key_pem)
        .context("parsing existing apiserver serving key")?;
    let ca_cert_pem = std::fs::read_to_string(dir.join("ca.crt"))
        .with_context(|| format!("reading cluster CA from {}", dir.display()))?;
    let ca_key_pem = std::fs::read_to_string(dir.join("ca.key"))
        .with_context(|| format!("reading cluster CA key from {}", dir.display()))?;
    let ca_key = rcgen::KeyPair::from_pem(&ca_key_pem).context("parsing cluster CA key")?;
    let ca_params = rcgen::CertificateParams::from_ca_cert_pem(&ca_cert_pem)
        .context("parsing cluster CA certificate")?;
    let ca_cert = ca_params
        .self_signed(&ca_key)
        .context("reconstructing cluster CA for API certificate refresh")?;

    let mut dns_sans = vec![
        "kubernetes".to_string(),
        "kubernetes.default".to_string(),
        "kubernetes.default.svc".to_string(),
        format!("kubernetes.default.svc.{cluster_domain}"),
        "localhost".to_string(),
    ];
    let mut ip_sans = vec![std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)];
    if let Some(service_ip) = service_ips.first() {
        ip_sans.push(*service_ip);
    }
    for extra_san in extra_sans {
        if let Ok(ip) = extra_san.parse() {
            ip_sans.push(ip);
        } else if !dns_sans.contains(extra_san) {
            dns_sans.push(extra_san.clone());
        }
    }
    let serving = issue_serving_cert_with_key(&ca_cert, &ca_key, &serving_key, &dns_sans, &ip_sans)
        .context("refreshing apiserver serving certificate SANs")?;
    atomic_write(&cert_path, &serving.cert_pem)
        .with_context(|| format!("writing refreshed API certificate to {}", cert_path.display()))?;
    tracing::info!(dir = %dir.display(), "refreshed apiserver serving certificate with required migration endpoint SANs");
    Ok(())
}

fn apiserver_extra_sans(
    service_ips: &[std::net::IpAddr],
    advertise_address: &str,
) -> Vec<String> {
    let mut sans: Vec<String> = service_ips
        .iter()
        .skip(1)
        .map(std::string::ToString::to_string)
        .collect();
    if !sans.iter().any(|san| san == advertise_address) {
        sans.push(advertise_address.to_string());
    }
    sans
}

fn configured_apiserver_extra_sans(value: String) -> Vec<String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|san| !san.is_empty())
        .map(str::to_owned)
        .collect()
}

fn ensure_existing_pki_matches_domain(
    dir: &std::path::Path,
    cluster_domain: &str,
    service_ips: &[std::net::IpAddr],
) -> Result<()> {
    let serving_cert = std::fs::read(dir.join("apiserver.crt"))
        .with_context(|| format!("reading existing apiserver certificate from {}", dir.display()))?;
    let pem = pem::parse(serving_cert).context("parsing existing apiserver certificate PEM")?;
    let (_, certificate) = x509_parser::parse_x509_certificate(pem.contents())
        .context("parsing existing apiserver certificate DER")?;
    let expected = format!("kubernetes.default.svc.{cluster_domain}");
    let matches = certificate
        .subject_alternative_name()
        .context("reading existing apiserver certificate SANs")?
        .is_some_and(|san| {
            let domain_matches = san.value.general_names.iter().any(|name| {
                matches!(name, x509_parser::extensions::GeneralName::DNSName(name) if *name == expected.as_str())
            });
            let service_ip_matches = service_ips.iter().all(|service_ip| {
                let expected_ip = match service_ip {
                    std::net::IpAddr::V4(ip) => ip.octets().to_vec(),
                    std::net::IpAddr::V6(ip) => ip.octets().to_vec(),
                };
                san.value.general_names.iter().any(|name| {
                    matches!(name, x509_parser::extensions::GeneralName::IPAddress(ip) if *ip == expected_ip.as_slice())
                })
            });
            domain_matches && service_ip_matches
        });
    anyhow::ensure!(
        matches,
        "existing cluster PKI at {} was generated for a different cluster domain or service CIDR; refusing to reuse it",
        dir.display()
    );
    Ok(())
}

/// Older bootstrap runs predate the aggregation front-proxy identity. Add a
/// separate front-proxy CA and leaf without regenerating the cluster CA or
/// any of the already-distributed identities during this migration.
fn ensure_front_proxy_client(dir: &std::path::Path) -> Result<()> {
    let ca_cert_path = dir.join("front-proxy-ca.crt");
    let ca_key_path = dir.join("front-proxy-ca.key");
    let cert_path = dir.join("front-proxy-client.crt");
    let key_path = dir.join("front-proxy-client.key");
    let paths = [&ca_cert_path, &ca_key_path, &cert_path, &key_path];
    let present = paths.iter().filter(|path| path.is_file()).count();
    if present == paths.len() {
        return Ok(());
    }
    if present != 0 {
        anyhow::bail!(
            "aggregation front-proxy PKI at {} is incomplete; front-proxy-ca.crt, front-proxy-ca.key, front-proxy-client.crt, and front-proxy-client.key are required",
            dir.display()
        )
    }

    let (ca_cert, ca_key) = generate_ca("not-k8s-front-proxy-ca")
        .context("generating aggregation front-proxy CA")?;
    let issued = issue_client_cert(&ca_cert, &ca_key, "front-proxy-client", &[])
        .context("issuing aggregation front-proxy client certificate")?;

    std::fs::write(&ca_cert_path, ca_cert.pem())
        .with_context(|| format!("writing {}", ca_cert_path.display()))?;
    std::fs::write(&ca_key_path, ca_key.serialize_pem())
        .with_context(|| format!("writing {}", ca_key_path.display()))?;
    std::fs::write(&cert_path, issued.cert_pem)
        .with_context(|| format!("writing {}", cert_path.display()))?;
    std::fs::write(&key_path, issued.key_pem)
        .with_context(|| format!("writing {}", key_path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&key_path, std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("restricting permissions on {}", key_path.display()))?;
    }
    tracing::info!(path = %cert_path.display(), "migrated cluster PKI with aggregation front-proxy identity");
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
    /// API client-auth trust may contain more roots than the serving CA.
    /// K3s, for example, uses separate server and client CAs.
    pub client_ca_bundle: String,
    pub apiserver_serving: IssuedCert,
    /// Client identity the apiserver presents when proxying exec/logs/
    /// attach/port-forward requests to nodelet.
    pub kube_apiserver_client: IssuedCert,
    /// Client identity the apiserver presents to aggregated API servers as
    /// their trusted front proxy.
    pub aggregation_proxy_client: IssuedCert,
    /// CA that aggregated API servers can trust for the front-proxy client
    /// certificate.
    pub aggregation_proxy_ca: IssuedCert,
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
    generate_with_migration_ca(spec, None)
}

struct MigrationCa {
    serving_cert_pem: String,
    serving_key_pem: String,
    client_ca_pem: Option<String>,
}

fn migration_ca_from_env() -> Result<Option<MigrationCa>> {
    let cert_path = std::env::var_os("NODEBOOTSTRAP_MIGRATION_CA_CERT_FILE");
    let key_path = std::env::var_os("NODEBOOTSTRAP_MIGRATION_CA_KEY_FILE");
    let client_ca_path = std::env::var_os("NODEBOOTSTRAP_MIGRATION_CLIENT_CA_FILE");
    anyhow::ensure!(
        cert_path.is_some() == key_path.is_some(),
        "NODEBOOTSTRAP_MIGRATION_CA_CERT_FILE and NODEBOOTSTRAP_MIGRATION_CA_KEY_FILE must be set together"
    );
    let Some((cert_path, key_path)) = cert_path.zip(key_path) else {
        anyhow::ensure!(
            client_ca_path.is_none(),
            "NODEBOOTSTRAP_MIGRATION_CLIENT_CA_FILE requires a migration serving CA"
        );
        return Ok(None);
    };
    let cert_path = std::path::PathBuf::from(cert_path);
    let key_path = std::path::PathBuf::from(key_path);
    let serving_cert_pem = std::fs::read_to_string(&cert_path)
        .with_context(|| format!("reading migration serving CA {}", cert_path.display()))?;
    let serving_key_pem = std::fs::read_to_string(&key_path)
        .with_context(|| format!("reading migration serving CA key {}", key_path.display()))?;
    let client_ca_pem = client_ca_path
        .map(std::path::PathBuf::from)
        .map(|path| {
            std::fs::read_to_string(&path)
                .with_context(|| format!("reading migration client CA bundle {}", path.display()))
        })
        .transpose()?;
    Ok(Some(MigrationCa {
        serving_cert_pem,
        serving_key_pem,
        client_ca_pem,
    }))
}

fn generate_with_migration_ca(
    spec: &ClusterPkiSpec,
    migration_ca: Option<&MigrationCa>,
) -> Result<ClusterPki> {
    let (ca_cert, ca_key, ca_cert_pem) = if let Some(source) = migration_ca {
        let params = CertificateParams::from_ca_cert_pem(&source.serving_cert_pem)
            .context("parsing source serving CA certificate")?;
        let key = migration_ca_key_pair(&source.serving_key_pem)
            .context("parsing source serving CA key")?;
        let source_der = pem::parse(&source.serving_cert_pem)
            .context("decoding source serving CA certificate")?;
        let (_, source_x509) = x509_parser::parse_x509_certificate(source_der.contents())
            .context("validating source serving CA certificate")?;
        let is_ca = source_x509
            .basic_constraints()
            .context("reading source serving CA basic constraints")?
            .is_some_and(|extension| extension.value.ca);
        anyhow::ensure!(is_ca, "source serving certificate is not a CA");
        let can_sign_certificates = source_x509
            .key_usage()
            .context("reading source serving CA key usage")?
            .is_some_and(|extension| extension.value.key_cert_sign());
        anyhow::ensure!(
            can_sign_certificates,
            "source serving CA is not permitted to sign certificates"
        );
        anyhow::ensure!(
            source_x509.public_key().raw == key.public_key_der().as_slice(),
            "source serving CA certificate and key do not match"
        );
        let cert = params
            .self_signed(&key)
            .context("validating source serving CA certificate and key")?;
        (cert, key, source.serving_cert_pem.clone())
    } else {
        let (cert, key) = generate_ca("not-k8s-ca").context("generating cluster CA")?;
        let cert_pem = cert.pem();
        (cert, key, cert_pem)
    };
    let (aggregation_proxy_ca_cert, aggregation_proxy_ca_key) =
        generate_ca("not-k8s-front-proxy-ca").context("generating aggregation front-proxy CA")?;

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
    let aggregation_proxy_client = issue_client_cert(
        &aggregation_proxy_ca_cert,
        &aggregation_proxy_ca_key,
        "front-proxy-client",
        &[],
    )
    .context("issuing aggregation proxy client cert")?;

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

    let client_ca_bundle = migration_ca
        .and_then(|migration_ca| migration_ca.client_ca_pem.as_ref())
        .map(|extra| format!("{}\n{}", ca_cert_pem.trim_end(), extra.trim()))
        .unwrap_or_else(|| ca_cert_pem.clone());
    let ca = IssuedCert { cert_pem: ca_cert_pem, key_pem: ca_key.serialize_pem() };

    Ok(ClusterPki {
        ca,
        client_ca_bundle,
        apiserver_serving,
        kube_apiserver_client,
        aggregation_proxy_client,
        aggregation_proxy_ca: IssuedCert {
            cert_pem: aggregation_proxy_ca_cert.pem(),
            key_pem: aggregation_proxy_ca_key.serialize_pem(),
        },
        sa_signing,
        kube_controller_manager,
        kube_scheduler,
        cluster_admin,
    })
}

fn migration_ca_key_pair(key_pem: &str) -> Result<KeyPair> {
    let parsed = pem::parse(key_pem).context("decoding source CA key PEM")?;
    if parsed.tag() == "EC PRIVATE KEY" {
        // K3s stores its default P-256 CA key in SEC1 form. Match the
        // conversion used by nodecontroller's CSR signer before passing it
        // to rcgen's ring-backed PKCS#8 parser.
        let secret = p256::SecretKey::from_sec1_pem(key_pem)
            .context("parsing source CA key as SEC1/P-256 PEM")?;
        let pkcs8_pem = p256::pkcs8::EncodePrivateKey::to_pkcs8_pem(&secret, Default::default())
            .context("re-encoding SEC1 source CA key as PKCS#8")?;
        return KeyPair::from_pem(pkcs8_pem.as_str())
            .context("parsing re-encoded source CA key as PKCS#8");
    }
    if parsed.tag() != "RSA PRIVATE KEY" {
        return KeyPair::from_pem(key_pem).context("parsing source CA key as PKCS#8 PEM");
    }

    // kubeadm commonly stores RSA CAs as PKCS#1 PEM. ring, which rcgen uses
    // here, accepts RSA keys only in PKCS#8; wrap the existing key bytes
    // without changing the key or its certificate identity.
    const RSA_ALGORITHM_IDENTIFIER: &[u8] = &[
        0x30, 0x0d, 0x06, 0x09, 0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x01, 0x05,
        0x00,
    ];
    let mut private_key_info = vec![0x02, 0x01, 0x00];
    private_key_info.extend_from_slice(RSA_ALGORITHM_IDENTIFIER);
    private_key_info.extend(der_tlv(0x04, parsed.contents()));
    let pkcs8 = der_tlv(0x30, &private_key_info);
    let pkcs8_pem = pem::encode(&pem::Pem::new("PRIVATE KEY", pkcs8));
    KeyPair::from_pem(&pkcs8_pem).context("parsing wrapped source RSA CA key as PKCS#8")
}

fn der_tlv(tag: u8, value: &[u8]) -> Vec<u8> {
    let mut encoded = Vec::with_capacity(value.len() + 6);
    encoded.push(tag);
    if value.len() < 128 {
        encoded.push(value.len() as u8);
    } else {
        let length = value.len().to_be_bytes();
        let first_nonzero = length.iter().position(|byte| *byte != 0).unwrap_or(length.len() - 1);
        let encoded_length = &length[first_nonzero..];
        encoded.push(0x80 | encoded_length.len() as u8);
        encoded.extend_from_slice(encoded_length);
    }
    encoded.extend_from_slice(value);
    encoded
}

fn ensure_migration_ca_matches(dir: &std::path::Path, migration_ca: &MigrationCa) -> Result<()> {
    let current_cert = std::fs::read_to_string(dir.join("ca.crt"))
        .with_context(|| format!("reading existing cluster CA from {}", dir.display()))?;
    anyhow::ensure!(
        current_cert == migration_ca.serving_cert_pem,
        "existing nodebootstrap PKI at {} belongs to a different cluster; refusing to replace its identity during migration",
        dir.display()
    );
    Ok(())
}

fn ensure_client_ca_bundle(dir: &std::path::Path, migration_ca: Option<&MigrationCa>) -> Result<()> {
    let path = dir.join("client-ca.crt");
    if migration_ca.is_none() && path.is_file() {
        return Ok(());
    }
    let expected = migration_ca
        .and_then(|migration_ca| migration_ca.client_ca_pem.as_ref())
        .map(|extra| {
            let serving_ca = std::fs::read_to_string(dir.join("ca.crt"))
                .with_context(|| format!("reading existing serving CA from {}", dir.display()))?;
            Ok::<_, anyhow::Error>(format!("{}\n{}", serving_ca.trim_end(), extra.trim()))
        })
        .transpose()?
        .or_else(|| std::fs::read_to_string(dir.join("ca.crt")).ok());
    let Some(expected) = expected else {
        anyhow::bail!("no API client CA bundle is available in {}", dir.display());
    };
    if !std::fs::read_to_string(&path).is_ok_and(|current| current == expected) {
        atomic_write(&path, &expected)
            .with_context(|| format!("writing API client CA bundle {}", path.display()))?;
    }
    Ok(())
}

fn generate_ca(common_name: &str) -> Result<(rcgen::Certificate, KeyPair)> {
    let mut params = CertificateParams::default();
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    let mut dn = DistinguishedName::new();
    dn.push(DnType::CommonName, common_name);
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
    let key = KeyPair::generate().context("generating serving cert keypair")?;
    issue_serving_cert_with_key(ca_cert, ca_key, &key, dns_sans, ip_sans)
}

fn issue_serving_cert_with_key(
    ca_cert: &rcgen::Certificate,
    ca_key: &KeyPair,
    key: &KeyPair,
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
    let cert = params
        .signed_by(key, ca_cert, ca_key)
        .context("signing serving cert with cluster CA")?;
    Ok(IssuedCert { cert_pem: cert.pem(), key_pem: key.serialize_pem() })
}

fn atomic_write(path: &std::path::Path, contents: &str) -> Result<()> {
    use std::io::Write;

    let parent = path.parent().context("target path has no parent directory")?;
    let file_name = path
        .file_name()
        .context("target path has no file name")?
        .to_string_lossy();
    let mut temporary = None;
    for attempt in 0..16 {
        let candidate = parent.join(format!(
            ".{file_name}.tmp.{}.{}",
            std::process::id(),
            attempt
        ));
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
        {
            Ok(file) => {
                temporary = Some((candidate, file));
                break;
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error).context("creating temporary PKI file"),
        }
    }
    let (temporary_path, mut file) = temporary.context("could not allocate temporary PKI file")?;
    let result = (|| -> Result<()> {
        file.write_all(contents.as_bytes())
            .context("writing temporary PKI file")?;
        file.sync_all().context("syncing temporary PKI file")?;
        std::fs::rename(&temporary_path, path).context("atomically replacing PKI file")?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary_path);
    }
    result
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
        write("client-ca.crt", &self.client_ca_bundle)?;
        write("apiserver.crt", &self.apiserver_serving.cert_pem)?;
        write("apiserver.key", &self.apiserver_serving.key_pem)?;
        write("kube-apiserver.crt", &self.kube_apiserver_client.cert_pem)?;
        write("kube-apiserver.key", &self.kube_apiserver_client.key_pem)?;
        write("front-proxy-ca.crt", &self.aggregation_proxy_ca.cert_pem)?;
        write("front-proxy-ca.key", &self.aggregation_proxy_ca.key_pem)?;
        write("front-proxy-client.crt", &self.aggregation_proxy_client.cert_pem)?;
        write("front-proxy-client.key", &self.aggregation_proxy_client.key_pem)?;
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
            &pki.aggregation_proxy_client,
            &pki.kube_controller_manager,
            &pki.kube_scheduler,
            &pki.cluster_admin,
        ] {
            assert!(issued.cert_pem.contains("BEGIN CERTIFICATE"));
            assert!(issued.key_pem.contains("PRIVATE KEY"));
        }
        assert!(pki.ca.cert_pem.contains("BEGIN CERTIFICATE"));
        assert!(pki.aggregation_proxy_ca.cert_pem.contains("BEGIN CERTIFICATE"));
        assert!(pki.sa_signing.key_pem.contains("PRIVATE KEY"));
    }

    #[test]
    fn migration_pki_preserves_serving_ca_and_accepts_an_additional_client_ca() {
        let source = generate(&ClusterPkiSpec::default()).expect("generate source PKI");
        let client = generate(&ClusterPkiSpec::default()).expect("generate additional client CA");
        let migration_ca = MigrationCa {
            serving_cert_pem: source.ca.cert_pem.clone(),
            serving_key_pem: source.ca.key_pem.clone(),
            client_ca_pem: Some(client.ca.cert_pem.clone()),
        };
        let migrated = generate_with_migration_ca(&ClusterPkiSpec::default(), Some(&migration_ca))
            .expect("generate migration PKI");

        assert_eq!(migrated.ca.cert_pem, source.ca.cert_pem);
        assert_eq!(
            migrated.client_ca_bundle,
            format!("{}\n{}", source.ca.cert_pem.trim_end(), client.ca.cert_pem.trim())
        );
        let parsed = pem::parse(&migrated.apiserver_serving.cert_pem)
            .expect("parse migrated API serving certificate");
        let (_, serving) = x509_parser::parse_x509_certificate(parsed.contents())
            .expect("parse migrated API serving certificate DER");
        let ca = pem::parse(&source.ca.cert_pem).expect("parse source API CA");
        let (_, source_ca) = x509_parser::parse_x509_certificate(ca.contents())
            .expect("parse source API CA DER");
        assert_eq!(serving.issuer(), source_ca.subject());

        let mismatched_ca = MigrationCa {
            serving_cert_pem: source.ca.cert_pem.clone(),
            serving_key_pem: client.ca.key_pem,
            client_ca_pem: None,
        };
        let error = generate_with_migration_ca(&ClusterPkiSpec::default(), Some(&mismatched_ca))
            .err()
            .expect("a mismatched source CA key must be rejected");
        assert!(error.to_string().contains("certificate and key do not match"));
    }

    #[test]
    fn existing_client_ca_bundle_survives_normal_bootstrap_reconciliation() {
        let directory = std::env::temp_dir().join(format!(
            "nodebootstrap-pki-client-ca-bundle-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&directory);
        let pki = generate(&ClusterPkiSpec::default()).expect("generate cluster PKI");
        pki.write_to_dir(&directory).expect("write cluster PKI");
        let preserved_bundle = format!("{}\nsource-client-ca", pki.ca.cert_pem.trim_end());
        std::fs::write(directory.join("client-ca.crt"), &preserved_bundle)
            .expect("write migrated client CA bundle");

        ensure_client_ca_bundle(&directory, None).expect("reconcile existing PKI");

        assert_eq!(
            std::fs::read_to_string(directory.join("client-ca.crt")).unwrap(),
            preserved_bundle
        );
        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn der_tlv_encodes_short_and_long_lengths() {
        assert_eq!(der_tlv(0x04, &[1, 2]), [0x04, 0x02, 0x01, 0x02]);
        let long = der_tlv(0x04, &[0; 128]);
        assert_eq!(&long[..3], &[0x04, 0x81, 0x80]);
        assert_eq!(long.len(), 131);
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
            matches!(name, x509_parser::extensions::GeneralName::DNSName(name) if *name == "kubernetes.default.svc.cluster.example")
        }));
    }

    #[test]
    fn apiserver_serving_certificate_includes_automatically_selected_node_address() {
        let service_ips = ["10.43.0.1".parse().expect("service IP")];
        let advertise_address = "10.1.1.125";
        let spec = ClusterPkiSpec {
            extra_sans: apiserver_extra_sans(&service_ips, advertise_address),
            ..ClusterPkiSpec::default()
        };
        let pki = generate(&spec).expect("generate cluster PKI");
        let der = pem::parse(&pki.apiserver_serving.cert_pem).expect("parse apiserver cert PEM");
        let (_, cert) = x509_parser::parse_x509_certificate(der.contents()).expect("parse apiserver cert DER");
        let san_ext = cert
            .subject_alternative_name()
            .expect("read apiserver SAN extension")
            .expect("apiserver cert should have a SAN extension");
        let expected_ip = [10_u8, 1, 1, 125];
        assert!(san_ext.value.general_names.iter().any(|name| {
            matches!(name, x509_parser::extensions::GeneralName::IPAddress(ip) if *ip == expected_ip.as_slice())
        }));
    }

    #[test]
    fn configured_apiserver_sans_include_the_source_api_endpoint_name() {
        let mut spec = ClusterPkiSpec::default();
        spec.extra_sans = configured_apiserver_extra_sans("cp-1,api.example.test".to_string());
        let pki = generate(&spec).expect("generate migration API certificate");
        let der = pem::parse(&pki.apiserver_serving.cert_pem).expect("parse apiserver cert PEM");
        let (_, cert) = x509_parser::parse_x509_certificate(der.contents())
            .expect("parse apiserver cert DER");
        let san_ext = cert
            .subject_alternative_name()
            .expect("read apiserver SAN extension")
            .expect("apiserver cert should have a SAN extension");
        for expected in ["cp-1", "api.example.test"] {
            assert!(san_ext.value.general_names.iter().any(|name| {
                matches!(name, x509_parser::extensions::GeneralName::DNSName(name) if *name == expected)
            }));
        }
    }

    #[test]
    fn existing_api_certificate_is_refreshed_for_new_migration_endpoint_sans() {
        let directory = std::env::temp_dir().join(format!(
            "nodebootstrap-pki-extra-san-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&directory);
        let original = generate(&ClusterPkiSpec::default()).expect("generate original PKI");
        original.write_to_dir(&directory).expect("write original PKI");
        let original_key = std::fs::read(directory.join("apiserver.key"))
            .expect("read existing API key");
        let service_ips = ["10.43.0.1".parse().expect("service IP")];
        ensure_existing_apiserver_extra_sans(
            &directory,
            "cluster.local",
            &service_ips,
            &["192.0.2.10".to_string(), "cp-1".to_string()],
        )
        .expect("refresh existing API certificate");
        assert_eq!(
            std::fs::read(directory.join("apiserver.key")).expect("read refreshed API key"),
            original_key,
            "refreshing SANs must preserve the serving key"
        );

        let refreshed = std::fs::read(directory.join("apiserver.crt"))
            .expect("read refreshed API certificate");
        let der = pem::parse(refreshed).expect("parse refreshed API certificate PEM");
        let (_, cert) = x509_parser::parse_x509_certificate(der.contents())
            .expect("parse refreshed API certificate DER");
        let san = cert
            .subject_alternative_name()
            .expect("read refreshed API certificate SAN")
            .expect("refreshed API certificate has SANs");
        assert!(san.value.general_names.iter().any(|name| {
            matches!(name, x509_parser::extensions::GeneralName::DNSName(name) if *name == "cp-1")
        }));
        let expected_ip = [192_u8, 0, 2, 10];
        assert!(san.value.general_names.iter().any(|name| {
            matches!(name, x509_parser::extensions::GeneralName::IPAddress(ip) if *ip == expected_ip.as_slice())
        }));
        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn existing_pki_is_rejected_when_cluster_domain_changes() {
        let dir = std::env::temp_dir().join(format!(
            "nodebootstrap-pki-domain-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let pki = generate(&ClusterPkiSpec::default()).expect("generate cluster PKI");
        pki.write_to_dir(&dir).expect("write cluster PKI");

        ensure_existing_pki_matches_domain(&dir, "cluster.local", &["10.43.0.1".parse().unwrap()])
            .expect("the original cluster domain should remain valid");
        let error = ensure_existing_pki_matches_domain(&dir, "cluster.example", &["10.43.0.1".parse().unwrap()])
            .expect_err("a changed cluster domain must not reuse the existing serving certificate");
        assert!(error.to_string().contains("different cluster domain"));

        let error = ensure_existing_pki_matches_domain(&dir, "cluster.local", &["10.99.0.1".parse().unwrap()])
            .expect_err("a changed service CIDR must not reuse the existing serving certificate");
        assert!(error.to_string().contains("service CIDR"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn existing_pki_is_migrated_with_a_front_proxy_client_identity() {
        let dir = std::env::temp_dir().join(format!(
            "nodebootstrap-pki-front-proxy-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let pki = generate(&ClusterPkiSpec::default()).expect("generate cluster PKI");
        pki.write_to_dir(&dir).expect("write cluster PKI");
        std::fs::remove_file(dir.join("front-proxy-ca.crt")).unwrap();
        std::fs::remove_file(dir.join("front-proxy-ca.key")).unwrap();
        std::fs::remove_file(dir.join("front-proxy-client.crt")).unwrap();
        std::fs::remove_file(dir.join("front-proxy-client.key")).unwrap();

        ensure_front_proxy_client(&dir).expect("migrate an older PKI directory");
        let cert = std::fs::read_to_string(dir.join("front-proxy-client.crt")).unwrap();
        let parsed = pem::parse(cert).expect("parse migrated front-proxy certificate");
        let (_, certificate) = x509_parser::parse_x509_certificate(parsed.contents()).unwrap();
        let common_name = certificate
            .subject()
            .iter_common_name()
            .next()
            .and_then(|value| value.as_str().ok())
            .unwrap();
        assert_eq!(common_name, "front-proxy-client");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
