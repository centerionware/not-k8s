use super::context::E2eContext;
use super::skip_test;
use anyhow::{Context, Result};
use http::Request;
use k8s_openapi::api::authentication::v1::{TokenRequest, TokenRequestSpec};
use k8s_openapi::api::certificates::v1::CertificateSigningRequest;
use k8s_openapi::api::core::v1::ServiceAccount;
use k8s_openapi::api::rbac::v1::{ClusterRole, ClusterRoleBinding};
use kube::api::{Api, DeleteParams, ListParams, PostParams};
use kube::config::{AuthInfo, Context as KubeContext, Kubeconfig, NamedAuthInfo, NamedContext};
use secrecy::SecretString;
use serde_json::json;
use std::fs;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// This check is selected by the external-CNI workflow mode. A normal
/// single-node run intentionally skips it because flannel is expected there.
pub(super) async fn external_cni_mode_disables_flannel(context: &E2eContext) -> Result<()> {
    let cfg = crate::config::Config::from_env()?;
    if !cfg.without_flannel && cfg.cni_provider.as_deref() == Some("flannel") {
        return Err(skip_test(
            "flannel is enabled for this bootstrap; run the external-cni workflow mode to exercise --without-flannel",
        ));
    }
    anyhow::ensure!(
        cfg.cni_provider.is_none(),
        "external-CNI mode must not select an internally managed provider: {:?}",
        cfg.cni_provider
    );
    anyhow::ensure!(
        cfg.without_flannel,
        "external-CNI mode must persist the --without-flannel preference"
    );
    let nodes = kube::api::Api::<k8s_openapi::api::core::v1::Node>::all(context.client.clone())
        .list(&kube::api::ListParams::default())
        .await
        .context("checking that the external-CNI bootstrap still registered a node")?;
    anyhow::ensure!(!nodes.items.is_empty(), "external-CNI bootstrap registered no nodes");

    anyhow::ensure!(
        std::process::Command::new("systemctl")
            .args(["is-active", "--quiet", "nodelet"])
            .status()
            .is_ok_and(|status| status.success()),
        "nodelet is not active after the external-CNI bootstrap"
    );
    if std::process::Command::new("systemctl")
        .args(["is-active", "--quiet", "flanneld"])
        .status()
        .is_ok_and(|status| status.success())
    {
        anyhow::bail!("flanneld is active after --without-flannel");
    }
    if std::process::Command::new("systemctl")
        .args(["is-active", "--quiet", "nodeproxy"])
        .status()
        .is_ok_and(|status| status.success())
    {
        anyhow::bail!("nodeproxy is active after --proxy=none");
    }
    Ok(())
}

pub(super) async fn graceful_node_shutdown_manual_note(_context: &E2eContext) -> Result<()> {
    Err(skip_test(
        "graceful node shutdown requires a real systemd-logind PrepareForShutdown signal; manual verification is documented in the archived graceful_shutdown case",
    ))
}

pub(super) async fn tls_bootstrap_issues_a_real_client_certificate(
    context: &E2eContext,
) -> Result<()> {
    let cfg = crate::config::Config::from_env()?;
    let nodelet_bin = std::env::var_os("NOTK8S_NODELET_E2E_BINARY")
        .map(PathBuf::from)
        .unwrap_or_else(|| cfg.toolchain_dir().join("bin/nodelet"));
    if !nodelet_bin.is_file() {
        return Err(skip_test(format!(
            "nodelet binary is not installed at {}; provide NOTK8S_NODELET_E2E_BINARY",
            nodelet_bin.display()
        )));
    }
    let kubeconfig_path = std::env::var_os("KUBECONFIG")
        .map(PathBuf::from)
        .or_else(|| Some(cfg.kubeconfig_dir().join("admin.kubeconfig")))
        .context("no kubeconfig is available for the TLS bootstrap fixture")?;
    if !kubeconfig_path.is_file() {
        return Err(skip_test(format!(
            "bootstrap kubeconfig is not present at {}",
            kubeconfig_path.display()
        )));
    }

    let suffix = format!("{}-{}", std::process::id(), Instant::now().elapsed().as_nanos());
    let scratch = std::env::temp_dir().join(format!("nodebootstrap-tls-bootstrap-{suffix}"));
    fs::create_dir_all(&scratch)?;
    let service_account_name = format!("tls-bootstrap-{suffix}");
    let role_name = format!("tls-bootstrap-{suffix}");
    let binding_name = format!("tls-bootstrap-{suffix}");
    let node_name = format!("tls-bootstrap-{suffix}");
    let bootstrap_path = scratch.join("bootstrap.kubeconfig");
    let output_path = scratch.join("nodelet.kubeconfig");
    let log_path = scratch.join("nodelet.log");
    let csr_path = scratch.join("request.csr");

    let service_accounts: Api<ServiceAccount> =
        Api::namespaced(context.client.clone(), &context.namespace);
    let roles: Api<ClusterRole> = Api::all(context.client.clone());
    let bindings: Api<ClusterRoleBinding> = Api::all(context.client.clone());
    let csrs: Api<CertificateSigningRequest> = Api::all(context.client.clone());
    let service_account: ServiceAccount = serde_json::from_value(json!({
        "apiVersion": "v1",
        "kind": "ServiceAccount",
        "metadata": {"name": service_account_name}
    }))?;
    let role: ClusterRole = serde_json::from_value(json!({
        "apiVersion": "rbac.authorization.k8s.io/v1",
        "kind": "ClusterRole",
        "metadata": {"name": role_name},
        "rules": [{"apiGroups": ["certificates.k8s.io"], "resources": ["certificatesigningrequests"], "verbs": ["create", "get", "list", "watch"]}]
    }))?;
    let binding: ClusterRoleBinding = serde_json::from_value(json!({
        "apiVersion": "rbac.authorization.k8s.io/v1",
        "kind": "ClusterRoleBinding",
        "metadata": {"name": binding_name},
        "subjects": [{"kind": "ServiceAccount", "name": service_account_name, "namespace": context.namespace}],
        "roleRef": {"kind": "ClusterRole", "name": role_name, "apiGroup": "rbac.authorization.k8s.io"}
    }))?;
    service_accounts
        .create(&PostParams::default(), &service_account)
        .await?;
    roles.create(&PostParams::default(), &role).await?;
    bindings.create(&PostParams::default(), &binding).await?;

    let token_request = Request::builder()
        .method("POST")
        .uri(format!(
            "/api/v1/namespaces/{}/serviceaccounts/{}/token",
            context.namespace, service_account_name
        ))
        .header("Content-Type", "application/json")
        .body(serde_json::to_vec(&TokenRequest {
            metadata: Default::default(),
            spec: TokenRequestSpec {
                audiences: vec!["https://kubernetes.default.svc".to_owned()],
                bound_object_ref: None,
                expiration_seconds: Some(600),
            },
            status: None,
        })?)?;
    let token = context
        .client
        .request::<TokenRequest>(token_request)
        .await?
        .status
        .context("TokenRequest response had no status")?
        .token;

    let mut kubeconfig = Kubeconfig::read_from(&kubeconfig_path)?;
    let cluster_name = kubeconfig
        .clusters
        .first()
        .context("admin kubeconfig has no cluster")?
        .name
        .clone();
    kubeconfig.auth_infos = vec![NamedAuthInfo {
        name: "bootstrap".to_owned(),
        auth_info: Some(AuthInfo {
            token: Some(SecretString::from(token)),
            ..Default::default()
        }),
        other: Default::default(),
    }];
    kubeconfig.contexts = vec![NamedContext {
        name: "bootstrap".to_owned(),
        context: Some(KubeContext {
            cluster: cluster_name,
            user: Some("bootstrap".to_owned()),
            namespace: Some(context.namespace.clone()),
            extensions: None,
            other: Default::default(),
        }),
        other: Default::default(),
    }];
    kubeconfig.current_context = Some("bootstrap".to_owned());
    fs::write(&bootstrap_path, serde_yaml::to_string(&kubeconfig)?)?;

    let mut nodelet = Command::new(&nodelet_bin)
        .env_remove("KUBECONFIG")
        .env("NODELET_BOOTSTRAP_KUBECONFIG", &bootstrap_path)
        .env("NODELET_KUBECONFIG", &output_path)
        .env("NODELET_NODE_NAME", &node_name)
        .env("NODELET_RUNTIME", "mock")
        .env("RUST_LOG", "info")
        .stdout(Stdio::null())
        .stderr(Stdio::from(fs::File::create(&log_path)?))
        .spawn()
        .with_context(|| format!("starting {} for TLS bootstrap", nodelet_bin.display()))?;

    let mut csr_name_for_cleanup = None;
    let result = async {
        let deadline = Instant::now() + Duration::from_secs(40);
        let (csr_name, csr) = loop {
            let found = csrs
                .list(&ListParams::default())
                .await?
                .items
                .into_iter()
                .find_map(|csr| {
                    let name = csr.metadata.name.clone()?;
                    name.starts_with(&format!("nodelet-{node_name}-")).then_some((name, csr))
                });
            if let Some(found) = found {
                break found;
            }
            if Instant::now() >= deadline {
                anyhow::bail!("nodelet never submitted a TLS bootstrap CSR; log: {}", log_path.display());
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        };
        csr_name_for_cleanup = Some(csr_name.clone());
        anyhow::ensure!(
            csr.spec.signer_name == "kubernetes.io/kube-apiserver-client-kubelet",
            "bootstrap CSR used signer {:?}",
            csr.spec.signer_name
        );
        fs::write(&csr_path, &csr.spec.request.0)?;
        let subject = Command::new("openssl")
            .args(["req", "-noout", "-subject", "-in"])
            .arg(&csr_path)
            .output()?;
        anyhow::ensure!(subject.status.success(), "openssl could not parse the bootstrap CSR");
        anyhow::ensure!(
            String::from_utf8_lossy(&subject.stdout).contains(&format!("system:node:{node_name}")),
            "bootstrap CSR subject did not contain system:node:{node_name}: {}",
            String::from_utf8_lossy(&subject.stdout)
        );
        let approval = Request::builder()
            .method("PATCH")
            .uri(format!(
                "/apis/certificates.k8s.io/v1/certificatesigningrequests/{csr_name}/approval"
            ))
            .header("Content-Type", "application/merge-patch+json")
            .body(serde_json::to_vec(&json!({"status": {"conditions": [{"type": "Approved", "status": "True", "reason": "NodebootstrapE2e", "message": "approved by the nodebootstrap Rust e2e"}]}}))?)?;
        let _: serde_json::Value = context.client.request(approval).await?;
        context
            .wait_until("nodelet to write its issued client certificate kubeconfig", Duration::from_secs(40), || {
                let output_path = output_path.clone();
                async move {
                    Ok(fs::read_to_string(output_path)
                        .ok()
                        .is_some_and(|contents| contents.contains("client-certificate-data")))
                }
            })
            .await?;
        let log = fs::read_to_string(&log_path).unwrap_or_default();
        anyhow::ensure!(log.contains("issued client certificate"), "nodelet did not log successful TLS bootstrap: {log}");
        Ok(())
    }
    .await;
    let _ = nodelet.kill();
    let _ = nodelet.wait();
    if let Some(csr_name) = csr_name_for_cleanup {
        let _ = csrs.delete(&csr_name, &DeleteParams::default()).await;
    }
    let _ = bindings.delete(&binding_name, &DeleteParams::default()).await;
    let _ = roles.delete(&role_name, &DeleteParams::default()).await;
    let _ = service_accounts.delete(&service_account_name, &DeleteParams::default()).await;
    let _ = fs::remove_dir_all(&scratch);
    result
}
