//! TLS bootstrap (round 96): the CSR-based initial-client-cert-issuance
//! flow real kubelet runs when started with `--bootstrap-kubeconfig` — a
//! low-privilege credential (typically a bootstrap token) that can only
//! create `CertificateSigningRequest` objects, exchanged for a real
//! client-certificate kubeconfig once the apiserver's own
//! node-authorizer/csrapproving controller approves and signs it. Nodelet
//! never self-approves; approval is entirely the apiserver's job, same as
//! upstream. No-op (returns immediately) unless
//! `NODELET_BOOTSTRAP_KUBECONFIG` is set — see `config.rs`'s doc comment
//! on `bootstrap_kubeconfig`/`kubeconfig_out` for the full precedence and
//! scope-simplification notes (no automatic rotation before expiry yet).

use crate::config::Config;
use anyhow::{Context, Result};
use k8s_openapi::api::certificates::v1::{
    CertificateSigningRequest, CertificateSigningRequestCondition, CertificateSigningRequestSpec,
};
use k8s_openapi::ByteString;
use kube::api::{ObjectMeta, Api, PostParams};
use kube::config::{AuthInfo, Context as KubeContext, Kubeconfig, NamedAuthInfo, NamedCluster, NamedContext};
use rcgen::{CertificateParams, DistinguishedName, DnType, KeyPair};
use secrecy::SecretString;
use std::time::Duration;
use tracing::info;

const SIGNER_NAME: &str = "kubernetes.io/kube-apiserver-client-kubelet";

/// The CN/O real kubelet's own node-identity convention uses for a client
/// cert requested via this flow — the apiserver's node-authorizer
/// recognizes exactly this shape to grant node-scoped permissions.
pub(crate) fn node_identity_dn(node_name: &str) -> (String, String) {
    (format!("system:node:{node_name}"), "system:nodes".to_string())
}

/// Generate a fresh keypair and a PKCS#10 CSR (PEM) for this node's
/// identity. Returns `(csr_pem, private_key_pem)`.
pub(crate) fn generate_csr(node_name: &str) -> Result<(String, String)> {
    let (cn, o) = node_identity_dn(node_name);
    let mut dn = DistinguishedName::new();
    dn.push(DnType::CommonName, cn);
    dn.push(DnType::OrganizationName, o);
    let mut params = CertificateParams::default();
    params.distinguished_name = dn;

    let key_pair = KeyPair::generate().context("generating a private key for the bootstrap CSR")?;
    let csr = params.serialize_request(&key_pair).context("building the PKCS#10 CSR")?;
    let csr_pem = csr.pem().context("PEM-encoding the CSR")?;
    let key_pem = key_pair.serialize_pem();
    Ok((csr_pem, key_pem))
}

/// Build the `CertificateSigningRequest` object to submit — pure given the
/// already-generated CSR PEM, so it's testable without real crypto.
pub(crate) fn build_csr_object(node_name: &str, csr_pem: &str) -> CertificateSigningRequest {
    CertificateSigningRequest {
        metadata: ObjectMeta { generate_name: Some(format!("nodelet-{node_name}-")), ..Default::default() },
        spec: CertificateSigningRequestSpec {
            request: ByteString(csr_pem.as_bytes().to_vec()),
            signer_name: SIGNER_NAME.to_string(),
            usages: Some(vec!["digital signature".to_string(), "key encipherment".to_string(), "client auth".to_string()]),
            ..Default::default()
        },
        status: None,
    }
}

/// What a `CertificateSigningRequest`'s current status means for the
/// bootstrap flow — pure, so the polling loop's decision logic is
/// unit-testable without a live apiserver.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum CsrOutcome {
    Issued(String),
    Denied(String),
    Pending,
}

pub(crate) fn csr_outcome(conditions: &[CertificateSigningRequestCondition], certificate: Option<&ByteString>) -> CsrOutcome {
    if let Some(cert) = certificate {
        if let Ok(pem) = String::from_utf8(cert.0.clone()) {
            return CsrOutcome::Issued(pem);
        }
    }
    for cond in conditions {
        if (cond.type_ == "Denied" || cond.type_ == "Failed") && cond.status == "True" {
            return CsrOutcome::Denied(cond.message.clone().unwrap_or_else(|| cond.type_.clone()));
        }
    }
    CsrOutcome::Pending
}

/// Build the output kubeconfig: same cluster/server/CA the bootstrap
/// kubeconfig pointed at, but a brand-new user authenticating with the
/// issued client certificate instead of whatever low-privilege credential
/// the bootstrap kubeconfig carried.
pub(crate) fn build_output_kubeconfig(bootstrap: &Kubeconfig, client_cert_pem: &str, client_key_pem: &str) -> Result<Kubeconfig> {
    use base64::Engine;
    let cluster = bootstrap
        .clusters
        .first()
        .and_then(|c| c.cluster.clone())
        .context("bootstrap kubeconfig has no cluster entry")?;

    let b64 = base64::engine::general_purpose::STANDARD;
    Ok(Kubeconfig {
        clusters: vec![NamedCluster { name: "default".to_string(), cluster: Some(cluster), other: Default::default() }],
        auth_infos: vec![NamedAuthInfo {
            name: "nodelet".to_string(),
            auth_info: Some(AuthInfo {
                client_certificate_data: Some(b64.encode(client_cert_pem.as_bytes())),
                client_key_data: Some(SecretString::from(b64.encode(client_key_pem.as_bytes()))),
                ..Default::default()
            }),
            other: Default::default(),
        }],
        contexts: vec![NamedContext {
            name: "default".to_string(),
            context: Some(KubeContext {
                cluster: "default".to_string(),
                user: Some("nodelet".to_string()),
                namespace: None,
                extensions: None,
                other: Default::default(),
            }),
            other: Default::default(),
        }],
        current_context: Some("default".to_string()),
        ..Default::default()
    })
}

/// Run the bootstrap flow if configured. No-op if `cfg.bootstrap_kubeconfig`
/// is empty (feature disabled) or if `cfg.kubeconfig_out` already exists
/// (already bootstrapped — see the scope note on `kubeconfig_out` about
/// rotation not being implemented yet). On success, sets `$KUBECONFIG` to
/// `cfg.kubeconfig_out` so the caller's subsequent
/// `kube::Client::try_default()` picks up the freshly issued credentials.
pub async fn run(cfg: &Config) -> Result<()> {
    if cfg.bootstrap_kubeconfig.is_empty() {
        return Ok(());
    }
    if std::path::Path::new(&cfg.kubeconfig_out).exists() {
        info!(path = %cfg.kubeconfig_out, "TLS bootstrap: kubeconfig already present, reusing it");
        unsafe {
            std::env::set_var("KUBECONFIG", &cfg.kubeconfig_out);
        }
        return Ok(());
    }

    info!(bootstrap = %cfg.bootstrap_kubeconfig, "TLS bootstrap: no kubeconfig at kubeconfig_out yet, starting CSR flow");
    let bootstrap_kubeconfig =
        Kubeconfig::read_from(&cfg.bootstrap_kubeconfig).context("reading NODELET_BOOTSTRAP_KUBECONFIG")?;
    let bootstrap_client =
        kube::Client::try_from(bootstrap_kubeconfig.clone()).context("building a client from the bootstrap kubeconfig")?;

    let (csr_pem, key_pem) = generate_csr(&cfg.node_name)?;
    let csr_obj = build_csr_object(&cfg.node_name, &csr_pem);

    let api: Api<CertificateSigningRequest> = Api::all(bootstrap_client);
    let created = api.create(&PostParams::default(), &csr_obj).await.context("submitting the bootstrap CSR")?;
    let name = created.metadata.name.clone().context("apiserver did not return a CSR name")?;
    info!(csr = %name, "TLS bootstrap: CSR submitted, waiting for approval");

    let cert_pem = poll_for_issuance(&api, &name).await?;

    let out = build_output_kubeconfig(&bootstrap_kubeconfig, &cert_pem, &key_pem)?;
    let yaml = serde_yaml::to_string(&out).context("serializing the bootstrapped kubeconfig")?;
    if let Some(parent) = std::path::Path::new(&cfg.kubeconfig_out).parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    std::fs::write(&cfg.kubeconfig_out, yaml).with_context(|| format!("writing {}", cfg.kubeconfig_out))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&cfg.kubeconfig_out, std::fs::Permissions::from_mode(0o600));
    }
    info!(path = %cfg.kubeconfig_out, "TLS bootstrap: issued client certificate written");
    unsafe {
        std::env::set_var("KUBECONFIG", &cfg.kubeconfig_out);
    }
    Ok(())
}

async fn poll_for_issuance(api: &Api<CertificateSigningRequest>, name: &str) -> Result<String> {
    const POLL_INTERVAL: Duration = Duration::from_secs(3);
    const MAX_ATTEMPTS: u32 = 100; // ~5 minutes

    for _ in 0..MAX_ATTEMPTS {
        let csr = api.get(name).await.context("polling the bootstrap CSR")?;
        let status = csr.status.unwrap_or_default();
        match csr_outcome(status.conditions.as_deref().unwrap_or_default(), status.certificate.as_ref()) {
            CsrOutcome::Issued(pem) => return Ok(pem),
            CsrOutcome::Denied(reason) => anyhow::bail!("bootstrap CSR {name} was denied/failed: {reason}"),
            CsrOutcome::Pending => {
                tokio::time::sleep(POLL_INTERVAL).await;
            }
        }
    }
    anyhow::bail!("timed out waiting for bootstrap CSR {name} to be approved and issued")
}

#[cfg(test)]
#[path = "bootstrap_tests/pure_functions.rs"]
mod tests_pure_functions;
