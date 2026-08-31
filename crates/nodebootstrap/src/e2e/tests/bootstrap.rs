use super::context::E2eContext;
use super::skip_test;
use anyhow::{Context, Result};
use futures::StreamExt;
use http::Request;
use k8s_openapi::apiextensions_apiserver::pkg::apis::apiextensions::v1::CustomResourceDefinition;
use k8s_openapi::api::apps::v1::Deployment;
use k8s_openapi::api::authentication::v1::{
    TokenRequest, TokenRequestSpec, TokenReview, TokenReviewSpec,
};
use k8s_openapi::api::certificates::v1::CertificateSigningRequest;
use k8s_openapi::api::core::v1::{
    ConfigMap, Endpoints, LocalObjectReference, Node, ObjectReference, Pod, Secret, Service,
    ServiceAccount,
};
use k8s_openapi::api::scheduling::v1::PriorityClass;
use k8s_openapi::api::rbac::v1::{ClusterRole, ClusterRoleBinding};
use kube::Error as KubeError;
use kube::api::{Api, DeleteParams, DynamicObject, ListParams, Patch, PatchParams, PostParams, WatchEvent, WatchParams};
use kube::config::{AuthInfo, Context as KubeContext, Kubeconfig, NamedAuthInfo, NamedContext};
use kube::core::GroupVersionKind;
use kube::discovery::ApiResource;
use secrecy::SecretString;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::fs;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use futures::AsyncBufReadExt;

fn run_privileged_output(program: &str, args: &[&str]) -> Result<Output> {
    let uid = Command::new("id")
        .arg("-u")
        .output()
        .context("checking the e2e runner's uid")?;
    let uid = String::from_utf8_lossy(&uid.stdout).trim().to_string();
    let mut command = if uid == "0" {
        let mut command = Command::new(program);
        command.args(args);
        command
    } else {
        let mut command = Command::new("sudo");
        command.arg(program).args(args);
        command
    };
    command
        .output()
        .with_context(|| format!("running {program}"))
}

fn run_privileged(program: &str, args: &[&str]) -> Result<()> {
    let output = run_privileged_output(program, args)?;
    anyhow::ensure!(
        output.status.success(),
        "{program} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(())
}

fn systemd_service_available(name: &str) -> bool {
    Command::new("systemctl")
        .args(["show", name, "--property=LoadState", "--value"])
        .output()
        .is_ok_and(|output| {
            output.status.success() && String::from_utf8_lossy(&output.stdout).trim() == "loaded"
        })
}

fn crd_is_established(crd: &CustomResourceDefinition) -> bool {
    crd.status.as_ref().is_some_and(|status| {
        status.conditions.as_ref().is_some_and(|conditions| {
            conditions.iter().any(|condition| condition.type_ == "Established" && condition.status == "True")
        })
    })
}

struct NodeapiserverAuthenticationOverride {
    drop_in: PathBuf,
    token_file: PathBuf,
}

impl NodeapiserverAuthenticationOverride {
    fn install() -> Result<Self> {
        if !systemd_service_available("nodeapiserver.service") {
            return Err(skip_test(
                "nodeapiserver.service is unavailable; authentication checks need systemd",
            ));
        }

        let suffix = std::process::id();
        let token_file = std::env::temp_dir().join(format!("nodeapiserver-e2e-token-{suffix}.csv"));
        fs::write(
            &token_file,
            "nodeapiserver-e2e-token,nodeapiserver-e2e-user,nodeapiserver-e2e-uid,\"system:bootstrappers,system:nodes\"\n",
        )
        .with_context(|| format!("writing {}", token_file.display()))?;

        let drop_in_dir = Path::new("/etc/systemd/system/nodeapiserver.service.d");
        let drop_in = drop_in_dir.join(format!("nodebootstrap-e2e-{suffix}.conf"));
        let local_drop_in = std::env::temp_dir().join(format!("nodeapiserver-e2e-{suffix}.conf"));
        let contents = format!(
            "[Service]\nEnvironment=NODEAPISERVER_ANONYMOUS_AUTH=0\nEnvironment=NODEAPISERVER_TOKEN_AUTH_FILE={}\n",
            token_file.display()
        );
        fs::write(&local_drop_in, contents)
            .with_context(|| format!("writing {}", local_drop_in.display()))?;

        let guard = Self { drop_in, token_file };
        let drop_in_dir = drop_in_dir.to_string_lossy();
        let local_drop_in = local_drop_in.to_string_lossy();
        let drop_in = guard.drop_in.to_string_lossy();
        run_privileged("mkdir", &["-p", drop_in_dir.as_ref()])?;
        run_privileged(
            "install",
            &["-m", "0644", local_drop_in.as_ref(), drop_in.as_ref()],
        )?;
        let _ = fs::remove_file(local_drop_in.as_ref());
        run_privileged("systemctl", &["daemon-reload"])?;
        run_privileged("systemctl", &["reset-failed", "nodeapiserver.service"])?;
        run_privileged("systemctl", &["restart", "nodeapiserver.service"])?;
        Ok(guard)
    }

    fn install_rbac() -> Result<Self> {
        if !systemd_service_available("nodeapiserver.service") {
            return Err(skip_test(
                "nodeapiserver.service is unavailable; authorization checks need systemd",
            ));
        }

        let suffix = std::process::id();
        let token_file = std::env::temp_dir().join(format!("nodeapiserver-e2e-rbac-{suffix}.csv"));
        fs::write(
            &token_file,
            "nodeapiserver-e2e-denied,nodeapiserver-e2e-denied,nodeapiserver-e2e-denied,\n",
        )
        .with_context(|| format!("writing {}", token_file.display()))?;

        let drop_in_dir = Path::new("/etc/systemd/system/nodeapiserver.service.d");
        let drop_in = drop_in_dir.join(format!("nodebootstrap-e2e-rbac-{suffix}.conf"));
        let local_drop_in = std::env::temp_dir().join(format!("nodeapiserver-e2e-rbac-{suffix}.conf"));
        let contents = format!(
            "[Service]\nEnvironment=NODEAPISERVER_ANONYMOUS_AUTH=0\nEnvironment=NODEAPISERVER_ENFORCE_RBAC=1\nEnvironment=NODEAPISERVER_TOKEN_AUTH_FILE={}\n",
            token_file.display()
        );
        fs::write(&local_drop_in, contents)
            .with_context(|| format!("writing {}", local_drop_in.display()))?;

        let guard = Self { drop_in, token_file };
        let drop_in_dir = drop_in_dir.to_string_lossy();
        let local_drop_in = local_drop_in.to_string_lossy();
        let drop_in = guard.drop_in.to_string_lossy();
        run_privileged("mkdir", &["-p", drop_in_dir.as_ref()])?;
        run_privileged(
            "install",
            &["-m", "0644", local_drop_in.as_ref(), drop_in.as_ref()],
        )?;
        let _ = fs::remove_file(local_drop_in.as_ref());
        run_privileged("systemctl", &["daemon-reload"])?;
        run_privileged("systemctl", &["reset-failed", "nodeapiserver.service"])?;
        run_privileged("systemctl", &["restart", "nodeapiserver.service"])?;
        Ok(guard)
    }
}

impl Drop for NodeapiserverAuthenticationOverride {
    fn drop(&mut self) {
        let drop_in = self.drop_in.to_string_lossy();
        let _ = run_privileged("rm", &["-f", drop_in.as_ref()]);
        let _ = run_privileged("systemctl", &["daemon-reload"]);
        let _ = run_privileged("systemctl", &["restart", "nodeapiserver.service"]);
        let _ = fs::remove_file(&self.token_file);
    }
}

struct NodeapiserverAuthorizationWebhookOverride {
    drop_in: PathBuf,
}

impl NodeapiserverAuthorizationWebhookOverride {
    fn install(url: &str) -> Result<Self> {
        if !systemd_service_available("nodeapiserver.service") {
            return Err(skip_test(
                "nodeapiserver.service is unavailable; authorization webhook checks need systemd",
            ));
        }

        let suffix = std::process::id();
        let drop_in_dir = Path::new("/etc/systemd/system/nodeapiserver.service.d");
        let drop_in = drop_in_dir.join(format!("nodebootstrap-e2e-authz-webhook-{suffix}.conf"));
        let local_drop_in = std::env::temp_dir().join(format!("nodeapiserver-authz-webhook-{suffix}.conf"));
        let contents = format!(
            "[Service]\nEnvironment=NODEAPISERVER_ANONYMOUS_AUTH=1\nEnvironment=NODEAPISERVER_ENFORCE_RBAC=1\nEnvironment=NODEAPISERVER_AUTHORIZATION_WEBHOOK_URL={url}\n"
        );
        fs::write(&local_drop_in, contents)
            .with_context(|| format!("writing {}", local_drop_in.display()))?;

        let guard = Self { drop_in };
        let drop_in_dir = drop_in_dir.to_string_lossy();
        let local_drop_in = local_drop_in.to_string_lossy();
        let drop_in = guard.drop_in.to_string_lossy();
        run_privileged("mkdir", &["-p", drop_in_dir.as_ref()])?;
        run_privileged(
            "install",
            &["-m", "0644", local_drop_in.as_ref(), drop_in.as_ref()],
        )?;
        let _ = fs::remove_file(local_drop_in.as_ref());
        run_privileged("systemctl", &["daemon-reload"])?;
        run_privileged("systemctl", &["reset-failed", "nodeapiserver.service"])?;
        run_privileged("systemctl", &["restart", "nodeapiserver.service"])?;
        Ok(guard)
    }
}

impl Drop for NodeapiserverAuthorizationWebhookOverride {
    fn drop(&mut self) {
        let drop_in = self.drop_in.to_string_lossy();
        let _ = run_privileged("rm", &["-f", drop_in.as_ref()]);
        let _ = run_privileged("systemctl", &["daemon-reload"]);
        let _ = run_privileged("systemctl", &["restart", "nodeapiserver.service"]);
    }
}

struct NodeapiserverAuditLogOverride {
    drop_in: PathBuf,
    audit_log: PathBuf,
    max_backups: usize,
}

impl NodeapiserverAuditLogOverride {
    fn install() -> Result<Self> {
        Self::install_with_rotation(None, 0)
    }

    fn install_with_rotation(max_size_bytes: Option<u64>, max_backups: usize) -> Result<Self> {
        if !systemd_service_available("nodeapiserver.service") {
            return Err(skip_test(
                "nodeapiserver.service is unavailable; audit-log checks need systemd",
            ));
        }

        let suffix = std::process::id();
        let audit_log = std::env::temp_dir().join(format!("nodeapiserver-e2e-audit-{suffix}.log"));
        let _ = fs::remove_file(&audit_log);
        let drop_in_dir = Path::new("/etc/systemd/system/nodeapiserver.service.d");
        let drop_in = drop_in_dir.join(format!("nodebootstrap-audit-{suffix}.conf"));
        let local_drop_in = std::env::temp_dir().join(format!("nodeapiserver-audit-{suffix}.conf"));
        let mut contents = format!(
            "[Service]\nEnvironment=NODEAPISERVER_AUDIT_LOG_PATH={}\n",
            audit_log.display()
        );
        if let Some(max_size_bytes) = max_size_bytes {
            contents.push_str(&format!(
                "Environment=NODEAPISERVER_AUDIT_LOG_MAX_SIZE_BYTES={max_size_bytes}\n"
            ));
            contents.push_str(&format!(
                "Environment=NODEAPISERVER_AUDIT_LOG_MAX_BACKUPS={max_backups}\n"
            ));
        }
        fs::write(&local_drop_in, contents)
            .with_context(|| format!("writing {}", local_drop_in.display()))?;

        let guard = Self { drop_in, audit_log, max_backups };
        let drop_in_dir = drop_in_dir.to_string_lossy();
        let local_drop_in = local_drop_in.to_string_lossy();
        let drop_in = guard.drop_in.to_string_lossy();
        run_privileged("mkdir", &["-p", drop_in_dir.as_ref()])?;
        run_privileged("install", &["-m", "0644", local_drop_in.as_ref(), drop_in.as_ref()])?;
        let _ = fs::remove_file(local_drop_in.as_ref());
        run_privileged("systemctl", &["daemon-reload"])?;
        run_privileged("systemctl", &["reset-failed", "nodeapiserver.service"])?;
        run_privileged("systemctl", &["restart", "nodeapiserver.service"])?;
        Ok(guard)
    }
}

impl Drop for NodeapiserverAuditLogOverride {
    fn drop(&mut self) {
        let drop_in = self.drop_in.to_string_lossy();
        let _ = run_privileged("rm", &["-f", drop_in.as_ref()]);
        let _ = run_privileged("systemctl", &["daemon-reload"]);
        let _ = run_privileged("systemctl", &["restart", "nodeapiserver.service"]);
        let _ = fs::remove_file(&self.audit_log);
        for index in 1..=self.max_backups {
            let backup = PathBuf::from(format!("{}.{}", self.audit_log.display(), index));
            let _ = fs::remove_file(backup);
        }
    }
}

struct NodeapiserverAuditWebhookOverride {
    drop_in: PathBuf,
    policy_file: Option<PathBuf>,
}

impl NodeapiserverAuditWebhookOverride {
    fn install(url: &str) -> Result<Self> {
        Self::install_with_policy(url, None)
    }

    fn install_with_policy(url: &str, policy: Option<&str>) -> Result<Self> {
        if !systemd_service_available("nodeapiserver.service") {
            return Err(skip_test(
                "nodeapiserver.service is unavailable; audit-webhook checks need systemd",
            ));
        }

        let suffix = std::process::id();
        let policy_file = policy.map(|contents| {
            let path = std::env::temp_dir().join(format!("nodeapiserver-e2e-audit-policy-{suffix}.yaml"));
            (path, contents)
        });
        if let Some((path, contents)) = &policy_file {
            fs::write(path, contents).with_context(|| format!("writing {}", path.display()))?;
        }
        let drop_in_dir = Path::new("/etc/systemd/system/nodeapiserver.service.d");
        let drop_in = drop_in_dir.join(format!("nodebootstrap-audit-webhook-{suffix}.conf"));
        let local_drop_in = std::env::temp_dir().join(format!("nodeapiserver-audit-webhook-{suffix}.conf"));
        let mut contents = format!("[Service]\nEnvironment=NODEAPISERVER_AUDIT_WEBHOOK_URL={url}\n");
        if let Some((path, _)) = &policy_file {
            contents.push_str(&format!("Environment=NODEAPISERVER_AUDIT_POLICY_FILE={}\n", path.display()));
        }
        fs::write(&local_drop_in, contents)
        .with_context(|| format!("writing {}", local_drop_in.display()))?;

        let guard = Self {
            drop_in,
            policy_file: policy_file.map(|(path, _)| path),
        };
        let drop_in_dir = drop_in_dir.to_string_lossy();
        let local_drop_in = local_drop_in.to_string_lossy();
        let drop_in = guard.drop_in.to_string_lossy();
        run_privileged("mkdir", &["-p", drop_in_dir.as_ref()])?;
        run_privileged("install", &["-m", "0644", local_drop_in.as_ref(), drop_in.as_ref()])?;
        let _ = fs::remove_file(local_drop_in.as_ref());
        run_privileged("systemctl", &["daemon-reload"])?;
        run_privileged("systemctl", &["reset-failed", "nodeapiserver.service"])?;
        run_privileged("systemctl", &["restart", "nodeapiserver.service"])?;
        Ok(guard)
    }
}

impl Drop for NodeapiserverAuditWebhookOverride {
    fn drop(&mut self) {
        let drop_in = self.drop_in.to_string_lossy();
        let _ = run_privileged("rm", &["-f", drop_in.as_ref()]);
        let _ = run_privileged("systemctl", &["daemon-reload"]);
        let _ = run_privileged("systemctl", &["restart", "nodeapiserver.service"]);
        if let Some(policy_file) = &self.policy_file {
            let _ = fs::remove_file(policy_file);
        }
    }
}

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
    anyhow::ensure!(
        !nodes.items.is_empty(),
        "external-CNI bootstrap registered no nodes"
    );

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

pub(super) async fn bootstrap_persists_installation_flags(_context: &E2eContext) -> Result<()> {
    let cfg = crate::config::Config::from_env()?;
    let path = cfg.flags_path();
    let output = run_privileged_output("cat", &[&path.to_string_lossy()])?;
    if !output.status.success() {
        return Err(skip_test(format!(
            "persisted bootstrap flags are not readable at {}",
            path.display()
        )));
    }
    let flags = String::from_utf8_lossy(&output.stdout);
    if flags.trim().is_empty() {
        return Err(skip_test(
            "this cluster was installed without command-line flags; persistence has no flag choice to verify",
        ));
    }
    anyhow::ensure!(
        flags
            .lines()
            .any(|flag| flag.starts_with("--cluster-domain=")),
        "persisted bootstrap flags did not retain the explicitly supplied cluster domain: {flags}"
    );
    anyhow::ensure!(
        !flags.lines().any(|flag| flag == "--e2e"
            || flag.starts_with("--only=")
            || flag.starts_with("--shard=")),
        "one-shot e2e controls were persisted as installation flags: {flags}"
    );
    Ok(())
}

pub(super) async fn nodelet_service_has_cluster_dns_configured(
    _context: &E2eContext,
) -> Result<()> {
    let cfg = crate::config::Config::from_env()?;
    let output = run_privileged_output(
        "systemctl",
        &[
            "show",
            "nodelet.service",
            "--property=Environment",
            "--value",
        ],
    )?;
    if !output.status.success() {
        return Err(skip_test("nodelet.service environment requires systemd"));
    }
    let environment = String::from_utf8_lossy(&output.stdout);
    if cfg.disable_dns {
        anyhow::ensure!(
            !environment.contains("NODELET_CLUSTER_DNS=")
                && !environment.contains("NODELET_CLUSTER_DOMAIN="),
            "nodelet retained DNS configuration despite --disable-dns: {environment}"
        );
    } else {
        anyhow::ensure!(
            environment.contains(&format!("NODELET_CLUSTER_DNS={}", cfg.cluster_dns_ip())),
            "nodelet.service has no configured cluster DNS server: {environment}"
        );
        if let Some(cluster_dns_ip6) = cfg.cluster_dns_ip6() {
            anyhow::ensure!(
                environment.contains(&format!(",{cluster_dns_ip6}")),
                "nodelet.service has no configured IPv6 cluster DNS server: {environment}"
            );
        }
        anyhow::ensure!(
            environment.contains(&format!("NODELET_CLUSTER_DOMAIN={}", cfg.cluster_domain())),
            "nodelet.service has no configured cluster domain: {environment}"
        );
    }
    Ok(())
}

pub(super) async fn configured_service_cidrs_are_used(context: &E2eContext) -> Result<()> {
    let cfg = crate::config::Config::from_env()?;
    if cfg.disable_dns {
        return Err(skip_test(
            "CoreDNS was intentionally disabled by --disable-dns",
        ));
    }
    let services: Api<Service> = Api::namespaced(context.client.clone(), "kube-system");
    let service = services
        .get("kube-dns")
        .await
        .context("getting the CoreDNS Service")?;
    let service = serde_json::to_value(service)?;
    let actual = service
        .pointer("/spec/clusterIPs")
        .and_then(serde_json::Value::as_array)
        .context("CoreDNS Service has no spec.clusterIPs")?;
    let actual: Vec<&str> = actual
        .iter()
        .filter_map(serde_json::Value::as_str)
        .collect();
    for expected in cfg.cluster_dns_ips() {
        anyhow::ensure!(
            actual.contains(&expected.as_str()),
            "CoreDNS Service is missing configured DNS ClusterIP {expected}: {actual:?}"
        );
    }
    Ok(())
}

pub(super) async fn coredns_is_a_healthy_deployment(context: &E2eContext) -> Result<()> {
    if crate::config::Config::from_env()?.disable_dns {
        return Err(skip_test(
            "CoreDNS was intentionally disabled by --disable-dns",
        ));
    }
    let deployments: Api<Deployment> = Api::namespaced(context.client.clone(), "kube-system");
    let deployment = deployments
        .get("coredns")
        .await
        .context("getting the CoreDNS Deployment")?;
    let pod_spec = deployment
        .spec
        .as_ref()
        .and_then(|spec| spec.template.spec.as_ref())
        .context("CoreDNS Deployment has no Pod template")?;
    let container = pod_spec
        .containers
        .iter()
        .find(|container| container.name == "coredns")
        .context("CoreDNS Deployment has no coredns container")?;
    let container = serde_json::to_value(container)?;
    for (probe_name, path, port) in [
        ("livenessProbe", "/health", 8080),
        ("readinessProbe", "/ready", 8181),
    ] {
        let http_get = container
            .get(probe_name)
            .and_then(|probe| probe.get("httpGet"))
            .context("CoreDNS probe is not an HTTP probe")?;
        anyhow::ensure!(
            http_get.get("path").and_then(serde_json::Value::as_str) == Some(path),
            "CoreDNS {probe_name} does not use {path}: {http_get}"
        );
        let actual_port = http_get
            .get("port")
            .and_then(serde_json::Value::as_i64)
            .or_else(|| {
                http_get
                    .get("port")
                    .and_then(serde_json::Value::as_str)
                    .and_then(|port| port.parse().ok())
            });
        anyhow::ensure!(
            actual_port == Some(port),
            "CoreDNS {probe_name} does not use port {port}: {http_get}"
        );
    }
    context
        .wait_until(
            "CoreDNS Deployment to have an available replica",
            Duration::from_secs(90),
            || {
                let deployments = deployments.clone();
                async move {
                    Ok(deployments
                        .get("coredns")
                        .await?
                        .status
                        .and_then(|status| status.available_replicas)
                        .unwrap_or_default()
                        >= 1)
                }
            },
        )
        .await?;

    let pods: Api<Pod> = Api::namespaced(context.client.clone(), "kube-system");
    context
        .wait_until(
            "CoreDNS Pod to report Ready",
            Duration::from_secs(30),
            || {
                let pods = pods.clone();
                async move {
                    let pod = pods
                        .list(&ListParams::default().labels("k8s-app=kube-dns"))
                        .await?
                        .items
                        .into_iter()
                        .any(|pod| {
                            pod.status.as_ref().is_some_and(|status| {
                                status.phase.as_deref() == Some("Running")
                                    && status.container_statuses.as_ref().is_some_and(
                                        |containers| {
                                            !containers.is_empty()
                                                && containers
                                                    .iter()
                                                    .all(|container| container.ready)
                                        },
                                    )
                                    && status.conditions.as_ref().is_some_and(|conditions| {
                                        conditions.iter().any(|condition| {
                                            condition.type_ == "Ready" && condition.status == "True"
                                        })
                                    })
                            })
                        });
                    Ok(pod)
                }
            },
        )
        .await
}

pub(super) async fn nodeapiserver_target_is_serving(context: &E2eContext) -> Result<()> {
    let cfg = crate::config::Config::from_env()?;
    if !matches!(cfg.target, crate::config::Target::NodeApiserver) {
        return Err(skip_test(
            "the cluster is using the upstream apiserver target",
        ));
    }
    let nodeapiserver_active =
        match run_privileged_output("systemctl", &["is-active", "--quiet", "nodeapiserver"]) {
            Ok(output) => output.status.success(),
            Err(error) => {
                return Err(skip_test(format!(
                    "nodeapiserver service check requires systemd: {error}"
                )))
            }
        };
    anyhow::ensure!(nodeapiserver_active, "nodeapiserver.service is not active");
    let upstream_active =
        run_privileged_output("systemctl", &["is-active", "--quiet", "kube-apiserver"])
            .map(|output| output.status.success())
            .unwrap_or(false);
    anyhow::ensure!(
        !upstream_active,
        "the upstream kube-apiserver service is still active alongside nodeapiserver"
    );

    // A successful typed API request proves the kubeconfig trusts the
    // bootstrap CA-signed nodeapiserver certificate and that nodestore-backed
    // resource reads are live, not merely that the process is running.
    let services: Api<Service> = Api::namespaced(context.client.clone(), "default");
    let expected_cluster_ip = cfg.service_ip()?.to_string();
    let service = services
        .get("kubernetes")
        .await
        .context("reading nodeapiserver default/kubernetes Service")?;
    anyhow::ensure!(
        service.spec.as_ref().is_some_and(|spec| {
            spec.cluster_ip.as_deref() == Some(expected_cluster_ip.as_str())
                && spec.ports.iter().flatten().any(|port| port.port == 6443)
        }),
        "nodeapiserver default/kubernetes Service is missing the configured ClusterIP or port"
    );
    let endpoints: Api<Endpoints> = Api::namespaced(context.client.clone(), "default");
    anyhow::ensure!(
        endpoints
            .get("kubernetes")
            .await?
            .subsets
            .unwrap_or_default()
            .into_iter()
            .flat_map(|subset| subset.addresses.unwrap_or_default())
            .any(|address| address
                .ip
                .parse::<std::net::IpAddr>()
                .is_ok_and(|ip| !ip.is_unspecified())),
        "nodeapiserver default/kubernetes has no endpoint"
    );

    let readyz_url = format!(
        "{}/readyz?verbose",
        cfg.apiserver_server().trim_end_matches('/')
    );
    let readyz = Command::new("curl")
        .args(["-k", "-sS", "-f", "--max-time", "5", &readyz_url])
        .output()
        .context("checking nodeapiserver readiness")?;
    anyhow::ensure!(
        readyz.status.success()
            && String::from_utf8_lossy(&readyz.stdout).contains("[+]storage ok"),
        "nodeapiserver /readyz did not report a live storage check: {}{}",
        String::from_utf8_lossy(&readyz.stdout),
        String::from_utf8_lossy(&readyz.stderr)
    );

    // The target also has to mint the projected token nodelet/CoreDNS use,
    // and accept that token through TokenReview. This catches a listener
    // that merely answers certificate-authenticated bootstrap requests.
    let token_request = Request::builder()
        .method("POST")
        .uri("/api/v1/namespaces/kube-system/serviceaccounts/default/token")
        .header("Content-Type", "application/json")
        .body(serde_json::to_vec(&TokenRequest {
            metadata: Default::default(),
            spec: TokenRequestSpec {
                audiences: Vec::new(),
                bound_object_ref: None,
                expiration_seconds: Some(600),
            },
            status: None,
        })?)?;
    let token = context
        .client
        .request::<TokenRequest>(token_request)
        .await
        .context("requesting a ServiceAccount token from nodeapiserver")?
        .status
        .context("nodeapiserver TokenRequest response had no status")?
        .token;
    anyhow::ensure!(
        !token.is_empty(),
        "nodeapiserver returned an empty ServiceAccount token"
    );
    let token_review = Request::builder()
        .method("POST")
        .uri("/apis/authentication.k8s.io/v1/tokenreviews")
        .header("Content-Type", "application/json")
        .body(serde_json::to_vec(&TokenReview {
            metadata: Default::default(),
            spec: TokenReviewSpec {
                token: Some(token),
                audiences: None,
            },
            status: None,
        })?)?;
    let review = context
        .client
        .request::<TokenReview>(token_review)
        .await
        .context("reviewing a nodeapiserver ServiceAccount token")?;
    anyhow::ensure!(
        review
            .status
            .as_ref()
            .is_some_and(|status| status.authenticated == Some(true)),
        "nodeapiserver TokenReview did not authenticate its own token"
    );
    anyhow::ensure!(
        review
            .status
            .and_then(|status| status.user)
            .is_some_and(|user| user.username
                == Some("system:serviceaccount:kube-system:default".to_string())),
        "nodeapiserver TokenReview returned the wrong ServiceAccount identity"
    );

    // kubectl and client-go still use the legacy OpenAPI endpoint for
    // schema discovery. Requiring a non-empty Swagger document here catches
    // a listener that serves discovery and CRUD but leaves `/openapi/v2` as
    // a stub or accidentally returns an OpenAPI v3 document there.
    let openapi_v2 = context
        .client
        .request::<Value>(
            Request::builder()
                .method("GET")
                .uri("/openapi/v2")
                .body(Vec::new())?,
        )
        .await
        .context("reading nodeapiserver OpenAPI v2")?;
    anyhow::ensure!(
        openapi_v2["swagger"] == "2.0"
            && openapi_v2["paths"].as_object().is_some_and(|paths| !paths.is_empty())
            && openapi_v2["definitions"].as_object().is_some_and(|definitions| !definitions.is_empty()),
        "nodeapiserver OpenAPI v2 response was not a populated Swagger document"
    );
    Ok(())
}

pub(super) async fn nodeapiserver_enforces_node_restriction(context: &E2eContext) -> Result<()> {
    let cfg = crate::config::Config::from_env()?;
    if !matches!(cfg.target, crate::config::Target::NodeApiserver) {
        return Err(skip_test(
            "NodeRestriction checks are only exercised against nodeapiserver",
        ));
    }
    if !Command::new("openssl")
        .arg("version")
        .status()
        .is_ok_and(|status| status.success())
    {
        return Err(skip_test("NodeRestriction e2e needs openssl"));
    }

    let nodes: Api<Node> = Api::all(context.client.clone());
    let node = nodes
        .list(&ListParams::default())
        .await?
        .items
        .into_iter()
        .next()
        .context("the cluster has no Node for NodeRestriction e2e")?;
    let node_name = node
        .metadata
        .name
        .clone()
        .context("the Node has no name for NodeRestriction e2e")?;
    let node_uid = node
        .metadata
        .uid
        .clone()
        .context("the Node has no UID for NodeRestriction e2e")?;

    let scratch = std::env::temp_dir().join(format!(
        "nodeapiserver-node-restriction-{}",
        std::process::id()
    ));
    fs::create_dir_all(&scratch)?;
    let client_key = scratch.join("node.key");
    let client_csr = scratch.join("node.csr");
    let client_crt = scratch.join("node.crt");
    let client_ext = scratch.join("node.ext");
    let client_serial = scratch.join("node.srl");
    let ca_crt = cfg.pki_dir().join("ca.crt");
    let ca_key = cfg.pki_dir().join("ca.key");
    if !ca_crt.is_file() || !ca_key.is_file() {
        let _ = fs::remove_dir_all(&scratch);
        return Err(skip_test(
            "NodeRestriction e2e needs the nodeapiserver cluster CA key material",
        ));
    }

    let run_privileged_owned = |program: &str, args: &[String]| -> Result<Output> {
        let args = args.iter().map(String::as_str).collect::<Vec<_>>();
        run_privileged_output(program, &args)
    };
    let run_success = |program: &str, args: &[String]| -> Result<()> {
        let output = run_privileged_owned(program, args)?;
        anyhow::ensure!(
            output.status.success(),
            "{program} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        Ok(())
    };

    fs::write(
        &client_ext,
        "basicConstraints=critical,CA:false\nkeyUsage=critical,digitalSignature,keyEncipherment\nextendedKeyUsage=clientAuth\n",
    )?;
    run_success(
        "openssl",
        &[
            "req".to_string(),
            "-newkey".to_string(),
            "rsa:2048".to_string(),
            "-nodes".to_string(),
            "-keyout".to_string(),
            client_key.to_string_lossy().into_owned(),
            "-out".to_string(),
            client_csr.to_string_lossy().into_owned(),
            "-subj".to_string(),
            format!("/CN=system:node:{node_name}/O=system:nodes"),
        ],
    )?;
    run_success(
        "openssl",
        &[
            "x509".to_string(),
            "-req".to_string(),
            "-in".to_string(),
            client_csr.to_string_lossy().into_owned(),
            "-CA".to_string(),
            ca_crt.to_string_lossy().into_owned(),
            "-CAkey".to_string(),
            ca_key.to_string_lossy().into_owned(),
            "-CAcreateserial".to_string(),
            "-CAserial".to_string(),
            client_serial.to_string_lossy().into_owned(),
            "-out".to_string(),
            client_crt.to_string_lossy().into_owned(),
            "-days".to_string(),
            "1".to_string(),
            "-extfile".to_string(),
            client_ext.to_string_lossy().into_owned(),
        ],
    )?;

    let endpoint = cfg.apiserver_server();
    let cert = client_crt.to_string_lossy().into_owned();
    let key = client_key.to_string_lossy().into_owned();
    let ca = ca_crt.to_string_lossy().into_owned();
    let curl = |method: &str, url: &str, data: Option<&str>| -> Result<Output> {
        let mut args = vec![
            "-k".to_string(),
            "-sS".to_string(),
            "--max-time".to_string(),
            "10".to_string(),
            "--cert".to_string(),
            cert.clone(),
            "--key".to_string(),
            key.clone(),
            "--cacert".to_string(),
            ca.clone(),
            "-X".to_string(),
            method.to_string(),
            "-w".to_string(),
            "\n%{http_code}".to_string(),
            url.to_string(),
        ];
        if let Some(data) = data {
            let content_type = if method == "PATCH" {
                "application/merge-patch+json"
            } else {
                "application/json"
            };
            args.splice(
                12..12,
                [
                    "-H".to_string(),
                    format!("Content-Type: {content_type}"),
                    "--data-binary".to_string(),
                    data.to_string(),
                ],
            );
        }
        let refs = args.iter().map(String::as_str).collect::<Vec<_>>();
        run_privileged_output("curl", &refs)
    };
    let response = |output: Output| -> Result<(u16, String)> {
        anyhow::ensure!(
            output.status.success(),
            "curl failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let text = String::from_utf8_lossy(&output.stdout).into_owned();
        let (body, code) = text
            .rsplit_once('\n')
            .context("curl response did not contain an HTTP status")?;
        Ok((code.trim().parse()?, body.to_string()))
    };

    let pod_name = format!("nodeapiserver-mirror-{}", std::process::id());
    let pod_url = format!(
        "{}/api/v1/namespaces/{}/pods",
        endpoint.trim_end_matches('/'),
        context.namespace
    );
    let mirror_pod = json!({
        "apiVersion": "v1",
        "kind": "Pod",
        "metadata": {
            "name": &pod_name,
            "namespace": &context.namespace,
            "annotations": {"kubernetes.io/config.mirror": "nodeapiserver-e2e"},
            "ownerReferences": [{
                "apiVersion": "v1",
                "kind": "Node",
                "name": &node_name,
                "uid": &node_uid,
                "controller": true
            }]
        },
        "spec": {
            "nodeName": &node_name,
            "restartPolicy": "Never",
            "containers": [{"name": "app", "image": "example.invalid/node-restriction-e2e"}]
        }
    });
    let (code, body) = response(curl("POST", &pod_url, Some(&serde_json::to_string(&mirror_pod)?))?)?;
    anyhow::ensure!(
        code == 201,
        "node identity could not create a valid mirror Pod (HTTP {code}): {body}"
    );

    let node_url = format!(
        "{}/api/v1/nodes/{node_name}",
        endpoint.trim_end_matches('/')
    );
    let (code, body) = response(curl(
        "PATCH",
        &node_url,
        Some(r#"{"metadata":{"labels":{"node-restriction.kubernetes.io/blocked":"true"}}}"#),
    )?)?;
    anyhow::ensure!(
        code == 403 && body.contains("node-restriction.kubernetes.io/blocked"),
        "NodeRestriction did not reject a forbidden node label (HTTP {code}): {body}"
    );

    let pod_delete_url = format!("{pod_url}/{pod_name}");
    let (code, body) = response(curl("DELETE", &pod_delete_url, None)?)?;
    let pods: Api<Pod> = Api::namespaced(context.client.clone(), &context.namespace);
    let _ = pods.delete(&pod_name, &DeleteParams::default()).await;
    let _ = fs::remove_dir_all(&scratch);
    anyhow::ensure!(
        code == 200 || code == 202,
        "node identity could not delete its own mirror Pod (HTTP {code}): {body}"
    );
    Ok(())
}

pub(super) async fn nodeapiserver_applies_core_defaults(context: &E2eContext) -> Result<()> {
    let cfg = crate::config::Config::from_env()?;
    if !matches!(cfg.target, crate::config::Target::NodeApiserver) {
        return Err(skip_test(
            "core defaulting checks are only exercised against nodeapiserver",
        ));
    }

    // Keep this object unschedulable by using an image that is never pulled;
    // the response is enough to prove the apiserver defaulted the object, and
    // deletion immediately removes the nodelet work item.
    let name = format!("nodeapiserver-defaults-{}", std::process::id());
    let pods: Api<Pod> = Api::namespaced(context.client.clone(), "default");
    let pod: Pod = serde_json::from_value(json!({
        "apiVersion": "v1",
        "kind": "Pod",
        "metadata": {"name": name},
        "spec": {
            "containers": [{"name": "app", "image": "example.invalid/not-k8s-defaults"}]
        }
    }))?;
    let created = pods
        .create(&PostParams::default(), &pod)
        .await
        .context("creating a Pod to verify core defaulting")?;
    let delete_result = pods.delete(&name, &DeleteParams::default()).await;
    anyhow::ensure!(
        delete_result.is_ok(),
        "cleanup of the core-defaulting Pod failed: {:?}",
        delete_result.err()
    );

    let returned = serde_json::to_value(created)?;
    anyhow::ensure!(
        returned.pointer("/spec/dnsPolicy").and_then(Value::as_str) == Some("ClusterFirst")
            && returned.pointer("/spec/restartPolicy").and_then(Value::as_str) == Some("Always")
            && returned.pointer("/spec/enableServiceLinks").and_then(Value::as_bool) == Some(true)
            && returned.pointer("/spec/containers/0/imagePullPolicy").and_then(Value::as_str)
                == Some("Always")
            && returned.pointer("/spec/containers/0/terminationMessagePath").and_then(Value::as_str)
                == Some("/dev/termination-log"),
        "nodeapiserver did not apply the expected core Pod defaults: {returned}"
    );
    Ok(())
}

pub(super) async fn nodeapiserver_rejects_invalid_builtin_schema_constraints(
    context: &E2eContext,
) -> Result<()> {
    let cfg = crate::config::Config::from_env()?;
    if !matches!(cfg.target, crate::config::Target::NodeApiserver) {
        return Err(skip_test(
            "built-in schema constraint checks are only exercised against nodeapiserver",
        ));
    }

    let name = format!("nodeapiserver-invalid-secret-{}", std::process::id());
    let request = Request::builder()
        .method("POST")
        .uri(format!(
            "/api/v1/namespaces/{}/secrets",
            context.namespace
        ))
        .header("content-type", "application/json")
        .body(serde_json::to_vec(&json!({
            "apiVersion": "v1",
            "kind": "Secret",
            "metadata": {"name": name, "namespace": context.namespace},
            "data": {"token": "not-base64"}
        }))?)?;

    match context.client.request::<Value>(request).await {
        Err(KubeError::Api(error)) if error.code == 422 => {}
        Err(error) => anyhow::bail!(
            "invalid built-in schema constraint returned the wrong API error: {error}"
        ),
        Ok(value) => anyhow::bail!(
            "invalid built-in schema constraint was accepted: {value}"
        ),
    }

    // Quantity is published as a oneOf(string, number) schema. A boolean
    // here must be rejected by the same generic OpenAPI validator rather
    // than silently passing through because it is nested in a map.
    let name = format!("nodeapiserver-invalid-quantity-{}", std::process::id());
    let request = Request::builder()
        .method("POST")
        .uri(format!(
            "/api/v1/namespaces/{}/pods",
            context.namespace
        ))
        .header("content-type", "application/json")
        .body(serde_json::to_vec(&json!({
            "apiVersion": "v1",
            "kind": "Pod",
            "metadata": {"name": name, "namespace": context.namespace},
            "spec": {
                "containers": [{
                    "name": "app",
                    "image": "example.invalid/not-k8s-invalid-quantity",
                    "resources": {"requests": {"cpu": true}}
                }]
            }
        }))?)?;

    match context.client.request::<Value>(request).await {
        Err(KubeError::Api(error)) if error.code == 422 => Ok(()),
        Err(error) => anyhow::bail!(
            "invalid oneOf schema value returned the wrong API error: {error}"
        ),
        Ok(value) => anyhow::bail!("invalid oneOf schema value was accepted: {value}"),
    }
}

pub(super) async fn nodeapiserver_rejects_invalid_metadata_keys(
    context: &E2eContext,
) -> Result<()> {
    let cfg = crate::config::Config::from_env()?;
    if !matches!(cfg.target, crate::config::Target::NodeApiserver) {
        return Err(skip_test(
            "metadata validation checks are only exercised against nodeapiserver",
        ));
    }

    let name = format!("nodeapiserver-invalid-metadata-{}", std::process::id());
    let request = Request::builder()
        .method("POST")
        .uri(format!(
            "/api/v1/namespaces/{}/configmaps",
            context.namespace
        ))
        .header("content-type", "application/json")
        .body(serde_json::to_vec(&json!({
            "apiVersion": "v1",
            "kind": "ConfigMap",
            "metadata": {
                "name": name,
                "namespace": context.namespace,
                "labels": {"invalid/key/with/two/slashes": "value"}
            }
        }))?)?;

    match context.client.request::<Value>(request).await {
        Err(KubeError::Api(error)) if error.code == 422 => Ok(()),
        Err(error) => anyhow::bail!(
            "invalid metadata key returned the wrong API error: {error}"
        ),
        Ok(value) => anyhow::bail!("invalid metadata key was accepted: {value}"),
    }
}

pub(super) async fn nodeapiserver_defaults_ingress_class(context: &E2eContext) -> Result<()> {
    let cfg = crate::config::Config::from_env()?;
    if !matches!(cfg.target, crate::config::Target::NodeApiserver) {
        return Err(skip_test(
            "DefaultIngressClass checks are only exercised against nodeapiserver",
        ));
    }

    let suffix = std::process::id();
    let class_name = format!("nodeapiserver-default-{suffix}");
    let ingress_name = format!("nodeapiserver-ingress-{suffix}");
    let class_uri = "/apis/networking.k8s.io/v1/ingressclasses";
    let ingress_uri = format!(
        "/apis/networking.k8s.io/v1/namespaces/{}/ingresses",
        context.namespace
    );

    let class: Value = context
        .client
        .request(
            Request::builder()
                .method("POST")
                .uri(class_uri)
                .body(serde_json::to_vec(&json!({
                    "apiVersion": "networking.k8s.io/v1",
                    "kind": "IngressClass",
                    "metadata": {
                        "name": &class_name,
                        "annotations": {"ingressclass.kubernetes.io/is-default-class": "true"}
                    },
                    "spec": {"controller": "nodeapiserver.test/default-ingress"}
                }))?)?,
            )
        .await
        .context("creating the default IngressClass")?;
    anyhow::ensure!(
        class["metadata"]["name"] == class_name,
        "nodeapiserver returned the wrong IngressClass: {class}"
    );

    let result = async {
        let ingress: Value = context
            .client
            .request(
                Request::builder()
                    .method("POST")
                    .uri(&ingress_uri)
                    .body(serde_json::to_vec(&json!({
                        "apiVersion": "networking.k8s.io/v1",
                        "kind": "Ingress",
                        "metadata": {"name": &ingress_name, "namespace": &context.namespace},
                        "spec": {}
                    }))?)?,
                )
            .await
            .context("creating an Ingress without a class")?;
        anyhow::ensure!(
            ingress["spec"]["ingressClassName"] == class_name,
            "nodeapiserver did not default the IngressClass: {ingress}"
        );
        Ok::<(), anyhow::Error>(())
    }
    .await;

    let _ = context
        .client
        .request::<Value>(Request::builder().method("DELETE").uri(format!("{ingress_uri}/{ingress_name}")).body(Vec::new())?)
        .await;
    let _ = context
        .client
        .request::<Value>(Request::builder().method("DELETE").uri(format!("{class_uri}/{class_name}")).body(Vec::new())?)
        .await;
    result
}

pub(super) async fn nodeapiserver_adds_storage_protection_finalizer(context: &E2eContext) -> Result<()> {
    let cfg = crate::config::Config::from_env()?;
    if !matches!(cfg.target, crate::config::Target::NodeApiserver) {
        return Err(skip_test(
            "storage protection checks are only exercised against nodeapiserver",
        ));
    }

    let name = format!("nodeapiserver-protection-{}", std::process::id());
    let uri = "/api/v1/persistentvolumes";
    let object: Value = context
        .client
        .request(
            Request::builder()
                .method("POST")
                .uri(uri)
                .body(serde_json::to_vec(&json!({
                    "apiVersion": "v1",
                    "kind": "PersistentVolume",
                    "metadata": {"name": &name},
                    "spec": {
                        "capacity": {"storage": "1Gi"},
                        "accessModes": ["ReadWriteOnce"],
                        "persistentVolumeReclaimPolicy": "Retain",
                        "hostPath": {"path": "/tmp/nodeapiserver-storage-protection"}
                    }
                }))?,
            )
        )
        .await
        .context("creating a PersistentVolume for storage-protection admission")?;

    let result = anyhow::ensure!(
        object["metadata"]["finalizers"]
            .as_array()
            .is_some_and(|finalizers| finalizers.iter().any(|finalizer| finalizer == "kubernetes.io/pv-protection")),
        "nodeapiserver did not add the PV protection finalizer: {object}"
    );
    let _ = context
        .client
        .request::<Value>(Request::builder().method("DELETE").uri(format!("{uri}/{name}")).body(Vec::new())?)
        .await;
    result
}

pub(super) async fn nodeapiserver_binds_a_pod_through_binding_subresource(
    context: &E2eContext,
) -> Result<()> {
    let cfg = crate::config::Config::from_env()?;
    if !matches!(cfg.target, crate::config::Target::NodeApiserver) {
        return Err(skip_test(
            "Pod binding is a nodeapiserver-only subresource check",
        ));
    }

    let suffix = std::process::id();
    let pod_name = format!("nodeapiserver-binding-{suffix}");
    let pods: Api<Pod> = Api::namespaced(context.client.clone(), &context.namespace);
    let nodes: Api<Node> = Api::all(context.client.clone());
    let node_name = nodes
        .list(&ListParams::default())
        .await?
        .items
        .into_iter()
        .next()
        .and_then(|node| node.metadata.name)
        .context("the cluster has no node for the Pod binding check")?;
    let pod: Pod = serde_json::from_value(json!({
        "apiVersion": "v1",
        "kind": "Pod",
        "metadata": {"name": pod_name.clone(), "namespace": &context.namespace},
        "spec": {
            "restartPolicy": "Never",
            "schedulerName": "nodeapiserver-binding-check",
            "containers": [{"name": "app", "image": "busybox:latest", "command": ["sleep", "30"]}]
        }
    }))?;
    let created = pods
        .create(&PostParams::default(), &pod)
        .await
        .context("creating an unbound Pod for the binding check")?;
    let uid = created
        .metadata
        .uid
        .clone()
        .context("the created Pod has no UID")?;
    let binding = json!({
        "apiVersion": "v1",
        "kind": "Binding",
        "metadata": {
            "name": pod_name.clone(),
            "namespace": &context.namespace,
            "uid": uid
        },
        "target": {"apiVersion": "v1", "kind": "Node", "name": node_name.clone()}
    });
    let request = Request::builder()
        .method("POST")
        .uri(format!(
            "/api/v1/namespaces/{}/pods/{}/binding",
            context.namespace, pod_name
        ))
        .header("Content-Type", "application/json")
        .body(serde_json::to_vec(&binding)?)?;
    let response = context
        .client
        .request::<Value>(request)
        .await
        .context("binding a Pod through nodeapiserver")?;
    anyhow::ensure!(
        response["status"] == "Success",
        "Pod binding did not return a successful Status: {response}"
    );

    let result = context
        .wait_until("nodeapiserver-bound Pod to record its node", Duration::from_secs(30), || {
            let pods = pods.clone();
            let pod_name = pod_name.clone();
            let node_name = node_name.clone();
            async move {
                Ok(pods
                    .get(&pod_name)
                    .await?
                    .spec
                    .and_then(|spec| spec.node_name)
                    == Some(node_name))
            }
        })
        .await;
    let _ = pods.delete(&pod_name, &DeleteParams::default()).await;
    result
}

pub(super) async fn nodeapiserver_advertises_subresources(context: &E2eContext) -> Result<()> {
    let cfg = crate::config::Config::from_env()?;
    if !matches!(cfg.target, crate::config::Target::NodeApiserver) {
        return Err(skip_test("subresource discovery is a nodeapiserver-only check"));
    }

    let core_request = Request::builder().uri("/api/v1").body(Vec::new())?;
    let core: Value = context.client.request(core_request).await.context("reading core/v1 discovery")?;
    let core_resources = core
        .get("resources")
        .and_then(Value::as_array)
        .context("core/v1 discovery did not contain resources")?;
    let has_subresource = |name: &str| {
        core_resources.iter().any(|resource| resource.get("name").and_then(Value::as_str) == Some(name))
    };
    anyhow::ensure!(has_subresource("pods/status"), "core/v1 discovery omitted pods/status");
    anyhow::ensure!(has_subresource("pods/log"), "core/v1 discovery omitted pods/log");
    let pods_exec = core_resources
        .iter()
        .find(|resource| resource.get("name").and_then(Value::as_str) == Some("pods/exec"))
        .context("core/v1 discovery omitted pods/exec")?;
    anyhow::ensure!(
        pods_exec
            .get("verbs")
            .and_then(Value::as_array)
            .is_some_and(|verbs| verbs.iter().any(|verb| verb.as_str() == Some("connect"))),
        "core/v1 discovery omitted the connect verb for pods/exec"
    );

    let apps_request = Request::builder().uri("/apis/apps/v1").body(Vec::new())?;
    let apps: Value = context.client.request(apps_request).await.context("reading apps/v1 discovery")?;
    let apps_resources = apps
        .get("resources")
        .and_then(Value::as_array)
        .context("apps/v1 discovery did not contain resources")?;
    let deployment_scale = apps_resources
        .iter()
        .find(|resource| resource.get("name").and_then(Value::as_str) == Some("deployments/scale"))
        .context("apps/v1 discovery omitted deployments/scale")?;
    anyhow::ensure!(deployment_scale.get("kind").and_then(Value::as_str) == Some("Scale"), "apps/v1 deployments/scale did not report kind Scale");
    anyhow::ensure!(deployment_scale.get("group").and_then(Value::as_str) == Some("autoscaling"), "apps/v1 deployments/scale omitted response group autoscaling");
    anyhow::ensure!(deployment_scale.get("version").and_then(Value::as_str) == Some("v1"), "apps/v1 deployments/scale omitted response version v1");
    Ok(())
}

pub(super) async fn nodeapiserver_serves_workload_scale_subresource(context: &E2eContext) -> Result<()> {
    let cfg = crate::config::Config::from_env()?;
    if !matches!(cfg.target, crate::config::Target::NodeApiserver) {
        return Err(skip_test("workload scale checks are only exercised against nodeapiserver"));
    }

    let name = format!("nodeapiserver-scale-{}", std::process::id());
    let scale_uri = format!("/apis/apps/v1/namespaces/{}/deployments/{name}/scale", context.namespace);
    let deployment_uri = format!("/apis/apps/v1/namespaces/{}/deployments/{name}", context.namespace);
    let result = async {
        let deployment = json!({
            "apiVersion": "apps/v1",
            "kind": "Deployment",
            "metadata": {"name": name.clone(), "namespace": context.namespace.clone()},
            "spec": {
                "replicas": 1,
                "selector": {"matchLabels": {"app": name.clone()}},
                "template": {
                    "metadata": {"labels": {"app": name.clone()}},
                    "spec": {"containers": [{"name": "app", "image": "busybox:latest", "command": ["sleep", "3600"]}]}
                }
            }
        });
        let create = Request::builder()
            .method("POST")
            .uri(format!("/apis/apps/v1/namespaces/{}/deployments", context.namespace))
            .header("Content-Type", "application/json")
            .body(serde_json::to_vec(&deployment)?)?;
        let _: Value = context.client.request(create).await.context("creating the scale Deployment")?;

        let initial: Value = context
            .client
            .request(Request::builder().method("GET").uri(&scale_uri).body(Vec::new())?)
            .await
            .context("reading the Deployment Scale")?;
        anyhow::ensure!(initial["kind"] == "Scale" && initial["apiVersion"] == "autoscaling/v1", "scale GET returned the wrong GVK: {initial}");
        anyhow::ensure!(initial["spec"]["replicas"] == 1, "scale GET returned the wrong desired replica count: {initial}");
        let resource_version = initial["metadata"]["resourceVersion"].as_str().context("scale GET returned no resourceVersion")?.to_string();

        let replacement = json!({
            "apiVersion": "autoscaling/v1",
            "kind": "Scale",
            "metadata": {"name": name.clone(), "namespace": context.namespace.clone(), "resourceVersion": resource_version},
            "spec": {"replicas": 2}
        });
        let put = Request::builder()
            .method("PUT")
            .uri(&scale_uri)
            .header("Content-Type", "application/json")
            .body(serde_json::to_vec(&replacement)?)?;
        let updated: Value = context.client.request(put).await.context("updating the Deployment Scale")?;
        anyhow::ensure!(updated["spec"]["replicas"] == 2, "scale PUT returned the wrong desired replica count: {updated}");

        let parent: Value = context
            .client
            .request(Request::builder().method("GET").uri(&deployment_uri).body(Vec::new())?)
            .await
            .context("reading the scaled Deployment")?;
        anyhow::ensure!(parent["spec"]["replicas"] == 2, "scale PUT did not update the parent Deployment: {parent}");

        let patch = Request::builder()
            .method("PATCH")
            .uri(&scale_uri)
            .header("Content-Type", "application/merge-patch+json")
            .body(br#"{"spec":{"replicas":1}}"#.to_vec())?;
        let patched: Value = context.client.request(patch).await.context("patching the Deployment Scale")?;
        anyhow::ensure!(patched["spec"]["replicas"] == 1, "scale PATCH returned the wrong desired replica count: {patched}");

        let final_parent: Value = context
            .client
            .request(Request::builder().method("GET").uri(&deployment_uri).body(Vec::new())?)
            .await
            .context("reading the Deployment after scale PATCH")?;
        anyhow::ensure!(final_parent["spec"]["replicas"] == 1, "scale PATCH did not update the parent Deployment: {final_parent}");
        Ok::<(), anyhow::Error>(())
    }
    .await;

    let _ = context
        .client
        .request::<Value>(Request::builder().method("DELETE").uri(&deployment_uri).body(Vec::new())?)
        .await;
    result
}

pub(super) async fn nodeapiserver_reconciles_managed_fields_across_versions(context: &E2eContext) -> Result<()> {
    let cfg = crate::config::Config::from_env()?;
    if !matches!(cfg.target, crate::config::Target::NodeApiserver) {
        return Err(skip_test("multi-version managed-fields checks are only exercised against nodeapiserver"));
    }

    let name = format!("nodeapiserver-ssa-{}", std::process::id());
    let uri_v1 = format!("/apis/autoscaling/v1/namespaces/{}/horizontalpodautoscalers/{name}", context.namespace);
    let uri_v2 = format!("/apis/autoscaling/v2/namespaces/{}/horizontalpodautoscalers/{name}", context.namespace);
    let first = json!({
        "apiVersion": "autoscaling/v1",
        "kind": "HorizontalPodAutoscaler",
        "metadata": {"name": name.clone(), "namespace": context.namespace.clone()},
        "spec": {
            "scaleTargetRef": {"apiVersion": "apps/v1", "kind": "Deployment", "name": "not-required-for-ssa"},
            "minReplicas": 1,
            "maxReplicas": 3,
            "targetCPUUtilizationPercentage": 50
        }
    });
    let first_request = Request::builder()
        .method("PATCH")
        .uri(format!("{uri_v1}?fieldManager=nodeapiserver-ssa-v1"))
        .header("Content-Type", "application/apply-patch+yaml")
        .body(serde_json::to_vec(&first)?)?;
    let created: Value = context
        .client
        .request(first_request)
        .await
        .context("applying the HPA through autoscaling/v1")?;
    anyhow::ensure!(
        created.pointer("/metadata/managedFields").and_then(Value::as_array).is_some_and(|entries| {
            entries.iter().any(|entry| {
                entry.get("manager").and_then(Value::as_str) == Some("nodeapiserver-ssa-v1")
                    && entry.get("apiVersion").and_then(Value::as_str) == Some("autoscaling/v1")
            })
        }),
        "autoscaling/v1 Apply did not record its API version in managedFields: {created}"
    );

    let second = json!({
        "apiVersion": "autoscaling/v2",
        "kind": "HorizontalPodAutoscaler",
        "metadata": {"name": name.clone(), "namespace": context.namespace.clone()},
        "spec": {
            "scaleTargetRef": {"apiVersion": "apps/v1", "kind": "Deployment", "name": "not-required-for-ssa"},
            "minReplicas": 1,
            "maxReplicas": 3,
            "metrics": [{"type": "Resource", "resource": {"name": "cpu", "target": {"type": "Utilization", "averageUtilization": 75}}}]
        }
    });
    let second_request = Request::builder()
        .method("PATCH")
        .uri(format!("{uri_v2}?fieldManager=nodeapiserver-ssa-v2"))
        .header("Content-Type", "application/apply-patch+yaml")
        .body(serde_json::to_vec(&second)?)?;
    match context.client.request::<Value>(second_request).await {
        Err(KubeError::Api(error)) if error.code == 409 => {}
        Err(error) => anyhow::bail!("cross-version Apply returned the wrong API error: {error}"),
        Ok(value) => anyhow::bail!("cross-version Apply unexpectedly took ownership: {value}"),
    }

    let third = json!({
        "apiVersion": "autoscaling/v2",
        "kind": "HorizontalPodAutoscaler",
        "metadata": {"name": name.clone(), "namespace": context.namespace.clone()},
        "spec": {
            "scaleTargetRef": {"apiVersion": "apps/v1", "kind": "Deployment", "name": "not-required-for-ssa"},
            "minReplicas": 1,
            "maxReplicas": 3
        }
    });
    let third_request = Request::builder()
        .method("PATCH")
        .uri(format!("{uri_v2}?fieldManager=nodeapiserver-ssa-v1"))
        .header("Content-Type", "application/apply-patch+yaml")
        .body(serde_json::to_vec(&third)?)?;
    let switched: Value = context
        .client
        .request(third_request)
        .await
        .context("re-applying the HPA through autoscaling/v2 with the original manager")?;
    anyhow::ensure!(
        switched.pointer("/spec/metrics").is_none(),
        "cross-version Apply did not prune the original manager's omitted metric: {switched}"
    );

    let _ = context
        .client
        .request::<Value>(Request::builder().method("DELETE").uri(uri_v1).body(Vec::new())?)
        .await;
    Ok(())
}

pub(super) async fn nodeapiserver_reconciles_crd_managed_fields_after_schema_change(context: &E2eContext) -> Result<()> {
    let cfg = crate::config::Config::from_env()?;
    if !matches!(cfg.target, crate::config::Target::NodeApiserver) {
        return Err(skip_test("CRD managed-fields schema changes are only exercised against nodeapiserver"));
    }

    let suffix = std::process::id();
    let group = format!("nodeapiserver-schema-{suffix}.test");
    let crd_name = format!("widgets.{group}");
    let widget_name = format!("widget-{suffix}");
    let crds: Api<CustomResourceDefinition> = Api::all(context.client.clone());
    let widgets: Api<DynamicObject> = Api::namespaced_with(
        context.client.clone(),
        &context.namespace,
        &ApiResource::from_gvk(&GroupVersionKind::gvk(&group, "v1", "Widget")),
    );
    let crd: CustomResourceDefinition = serde_json::from_value(json!({
        "apiVersion": "apiextensions.k8s.io/v1",
        "kind": "CustomResourceDefinition",
        "metadata": {"name": crd_name},
        "spec": {
            "group": group,
            "names": {
                "plural": "widgets",
                "singular": "widget",
                "kind": "Widget",
                "listKind": "WidgetList"
            },
            "scope": "Namespaced",
            "versions": [{
                "name": "v1",
                "served": true,
                "storage": true,
                "schema": {"openAPIV3Schema": {
                    "type": "object",
                    "properties": {"spec": {
                        "type": "object",
                        "properties": {"settings": {
                            "type": "object",
                            "x-kubernetes-map-type": "granular",
                            "additionalProperties": {"type": "string"}
                        }}
                    }}
                }}
            }]
        }
    }))?;

    let result = async {
        crds.create(&PostParams::default(), &crd)
            .await
            .context("creating the CRD schema-reconciliation fixture")?;
        context
            .wait_until("CRD schema-reconciliation fixture to become established", Duration::from_secs(60), || {
                let crds = crds.clone();
                let crd_name = crd_name.clone();
                async move { Ok(crds.get_opt(&crd_name).await?.is_some_and(|crd| crd_is_established(&crd))) }
            })
            .await?;

        let first: DynamicObject = serde_json::from_value(json!({
            "apiVersion": format!("{group}/v1"),
            "kind": "Widget",
            "metadata": {"name": widget_name.clone(), "namespace": &context.namespace},
            "spec": {"settings": {"a": "one"}}
        }))?;
        widgets
            .patch(
                &widget_name,
                &kube::api::PatchParams::apply("nodeapiserver-schema-a"),
                &Patch::Apply(first),
            )
            .await
            .context("applying the CRD object with a granular map")?;

        let mut updated = serde_json::to_value(&crd)?;
        updated["spec"]["versions"][0]["schema"]["openAPIV3Schema"]["properties"]["spec"]["properties"]["settings"]["x-kubernetes-map-type"] = json!("atomic");
        crds.patch(
            &crd_name,
            &PatchParams::default(),
            &Patch::Merge(json!({"spec": {"versions": updated["spec"]["versions"].clone()}})),
        )
            .await
            .context("changing the CRD map from granular to atomic")?;

        let second: DynamicObject = serde_json::from_value(json!({
            "apiVersion": format!("{group}/v1"),
            "kind": "Widget",
            "metadata": {"name": widget_name.clone(), "namespace": &context.namespace},
            "spec": {"settings": {"b": "two"}}
        }))?;
        match widgets
            .patch(
                &widget_name,
                &kube::api::PatchParams::apply("nodeapiserver-schema-b"),
                &Patch::Apply(second),
            )
            .await
        {
            Err(KubeError::Api(error)) if error.code == 409 => {}
            Err(error) => anyhow::bail!("post-schema-change Apply returned the wrong API error: {error}"),
            Ok(value) => anyhow::bail!("post-schema-change Apply did not conflict with the collapsed owner: {value:?}"),
        }
        Ok::<(), anyhow::Error>(())
    }
    .await;

    let _ = widgets.delete(&widget_name, &DeleteParams::default()).await;
    let _ = crds.delete(&crd_name, &DeleteParams::default()).await;
    result
}

pub(super) async fn nodeapiserver_authentication_modes(context: &E2eContext) -> Result<()> {
    let cfg = crate::config::Config::from_env()?;
    if !matches!(cfg.target, crate::config::Target::NodeApiserver) {
        return Err(skip_test(
            "authentication-mode checks are only exercised against nodeapiserver",
        ));
    }
    if Command::new("curl").arg("--version").output().is_err() {
        return Err(skip_test("curl is required for unauthenticated HTTPS checks"));
    }

    let auth_override = NodeapiserverAuthenticationOverride::install()?;
    context
        .wait_until(
            "nodeapiserver to become active after authentication configuration",
            Duration::from_secs(60),
            || async {
                Ok(Command::new("systemctl")
                    .args(["is-active", "--quiet", "nodeapiserver.service"])
                    .status()
                    .is_ok_and(|status| status.success()))
            },
        )
        .await?;

    context
        .wait_until(
            "nodeapiserver to answer requests after authentication configuration",
            Duration::from_secs(60),
            || async {
                let Ok(output) = Command::new("curl")
                    .args([
                        "-k",
                        "-sS",
                        "--max-time",
                        "2",
                        "-o",
                        "/dev/null",
                        "-w",
                        "%{http_code}",
                        "https://127.0.0.1:6443/healthz",
                    ])
                    .output()
                else {
                    return Ok(false);
                };
                Ok(output.status.success()
                    && String::from_utf8_lossy(&output.stdout).trim() != "000")
            },
        )
        .await?;

    let anonymous = Command::new("curl")
        .args([
            "-k",
            "-sS",
            "--max-time",
            "10",
            "-o",
            "/dev/null",
            "-w",
            "%{http_code}",
            "https://127.0.0.1:6443/healthz",
        ])
        .output()
        .context("checking disabled anonymous authentication")?;
    anyhow::ensure!(
        String::from_utf8_lossy(&anonymous.stdout).trim() == "401",
        "anonymous nodeapiserver request was not rejected: {}",
        String::from_utf8_lossy(&anonymous.stdout)
    );

    let authenticated = Command::new("curl")
        .args([
            "-k",
            "-sS",
            "--max-time",
            "10",
            "-H",
            "Authorization: Bearer nodeapiserver-e2e-token",
            "-H",
            "Content-Type: application/json",
            "-d",
            r#"{"apiVersion":"authentication.k8s.io/v1","kind":"SelfSubjectReview"}"#,
            "-w",
            "\n%{http_code}",
            "https://127.0.0.1:6443/apis/authentication.k8s.io/v1/selfsubjectreviews",
        ])
        .output()
        .context("checking static token authentication")?;
    let authenticated_body = String::from_utf8_lossy(&authenticated.stdout);
    anyhow::ensure!(
        authenticated.status.success()
            && authenticated_body.lines().last() == Some("201")
            && authenticated_body.contains("nodeapiserver-e2e-user")
            && authenticated_body.contains("nodeapiserver-e2e-uid"),
        "static token did not authenticate and populate SelfSubjectReview: {}",
        authenticated_body
    );

    fs::write(
        &auth_override.token_file,
        "nodeapiserver-e2e-rotated,nodeapiserver-e2e-rotated-user,nodeapiserver-e2e-rotated-uid,\n",
    )
    .with_context(|| format!("rotating {}", auth_override.token_file.display()))?;
    context
        .wait_until(
            "nodeapiserver to reload its static token file",
            Duration::from_secs(30),
            || async {
                let Ok(output) = Command::new("curl")
                    .args([
                        "-k",
                        "-sS",
                        "--max-time",
                        "10",
                        "-H",
                        "Authorization: Bearer nodeapiserver-e2e-rotated",
                        "-H",
                        "Content-Type: application/json",
                        "-d",
                        r#"{"apiVersion":"authentication.k8s.io/v1","kind":"SelfSubjectReview"}"#,
                        "-w",
                        "\n%{http_code}",
                        "https://127.0.0.1:6443/apis/authentication.k8s.io/v1/selfsubjectreviews",
                    ])
                    .output()
                else {
                    return Ok(false);
                };
                let body = String::from_utf8_lossy(&output.stdout);
                Ok(output.status.success()
                    && body.lines().last() == Some("201")
                    && body.contains("nodeapiserver-e2e-rotated-user"))
            },
        )
        .await?;

    let old_token = Command::new("curl")
        .args([
            "-k",
            "-sS",
            "--max-time",
            "10",
            "-o",
            "/dev/null",
            "-w",
            "%{http_code}",
            "-H",
            "Authorization: Bearer nodeapiserver-e2e-token",
            "https://127.0.0.1:6443/healthz",
        ])
        .output()
        .context("checking that the old static token was removed after reload")?;
    anyhow::ensure!(
        old_token.status.success()
            && String::from_utf8_lossy(&old_token.stdout).trim() == "401",
        "old static token remained valid after reload: {}",
        String::from_utf8_lossy(&old_token.stdout)
    );
    Ok(())
}

pub(super) async fn nodeapiserver_apf_labels_requests(context: &E2eContext) -> Result<()> {
    let cfg = crate::config::Config::from_env()?;
    if !matches!(cfg.target, crate::config::Target::NodeApiserver) {
        return Err(skip_test("API Priority and Fairness is a nodeapiserver-only check"));
    }

    let suffix = std::process::id();
    let priority_name = format!("nodeapiserver-e2e-{suffix}");
    let flow_name = format!("nodeapiserver-e2e-{suffix}");
    let priority = json!({
        "apiVersion": "flowcontrol.apiserver.k8s.io/v1",
        "kind": "PriorityLevelConfiguration",
        "metadata": {"name": priority_name},
        "spec": {
            "type": "Limited",
            "limited": {
                "nominalConcurrencyShares": 1,
                "limitResponse": {
                    "type": "Reject",
                    "queuing": {"queues": 1, "handSize": 1, "queueLengthLimit": 1}
                }
            }
        }
    });
    let flow = json!({
        "apiVersion": "flowcontrol.apiserver.k8s.io/v1",
        "kind": "FlowSchema",
        "metadata": {"name": flow_name},
        "spec": {
            "matchingPrecedence": 1,
            "priorityLevelConfiguration": {"name": priority_name},
            "rules": [{
                "subjects": [{"kind": "Group", "group": {"name": "system:authenticated"}}],
                "nonResourceRules": [{"verbs": ["get"], "nonResourceURLs": ["/version"]}]
            }]
        }
    });
    let priority_request = Request::builder()
        .method("POST")
        .uri("/apis/flowcontrol.apiserver.k8s.io/v1/prioritylevelconfigurations")
        .header("Content-Type", "application/json")
        .body(serde_json::to_vec(&priority)?)?;
    let priority: Value = context
        .client
        .request(priority_request)
        .await
        .context("creating the e2e PriorityLevelConfiguration")?;
    let flow_request = Request::builder()
        .method("POST")
        .uri("/apis/flowcontrol.apiserver.k8s.io/v1/flowschemas")
        .header("Content-Type", "application/json")
        .body(serde_json::to_vec(&flow)?)?;
    let flow: Value = context
        .client
        .request(flow_request)
        .await
        .context("creating the e2e FlowSchema")?;

    let result = async {
        let token_request = Request::builder()
            .method("POST")
            .uri(format!(
                "/api/v1/namespaces/{}/serviceaccounts/default/token",
                context.namespace
            ))
            .header("Content-Type", "application/json")
            .body(serde_json::to_vec(&json!({
                "apiVersion": "authentication.k8s.io/v1",
                "kind": "TokenRequest",
                "spec": {"expirationSeconds": 600}
            }))?)?;
        let token: TokenRequest = context.client.request(token_request).await?;
        let token = token.status.context("TokenRequest response had no status")?.token;
        let endpoint = format!("{}/version", cfg.apiserver_server().trim_end_matches('/'));
        let output = Command::new("curl")
            .args([
                "-k", "-sS", "--max-time", "10", "-D", "-", "-o", "/dev/null",
                "-H", &format!("Authorization: Bearer {token}"), &endpoint,
            ])
            .output()
            .context("calling nodeapiserver to inspect APF response headers")?;
        anyhow::ensure!(output.status.success(), "APF header request failed: {}", String::from_utf8_lossy(&output.stderr));
        let headers = String::from_utf8_lossy(&output.stdout).to_ascii_lowercase();
        let flow_uid = flow.pointer("/metadata/uid").and_then(Value::as_str).context("FlowSchema response had no UID")?;
        let priority_uid = priority.pointer("/metadata/uid").and_then(Value::as_str).context("PriorityLevelConfiguration response had no UID")?;
        anyhow::ensure!(
            headers.contains(&format!("x-kubernetes-pf-flowschema-uid: {flow_uid}").to_ascii_lowercase()),
            "APF response did not identify the selected FlowSchema: {headers}"
        );
        anyhow::ensure!(
            headers.contains(&format!("x-kubernetes-pf-prioritylevel-uid: {priority_uid}").to_ascii_lowercase()),
            "APF response did not identify the selected PriorityLevelConfiguration: {headers}"
        );
        Ok(())
    }
    .await;

    for (method, uri) in [
        ("DELETE", format!("/apis/flowcontrol.apiserver.k8s.io/v1/flowschemas/{flow_name}")),
        ("DELETE", format!("/apis/flowcontrol.apiserver.k8s.io/v1/prioritylevelconfigurations/{priority_name}")),
    ] {
        let request = Request::builder().method(method).uri(uri).body(Vec::new())?;
        let _ = context.client.request::<Value>(request).await;
    }
    result
}

pub(super) async fn nodeapiserver_exposes_inflight_metrics(_context: &E2eContext) -> Result<()> {
    let cfg = crate::config::Config::from_env()?;
    if !matches!(cfg.target, crate::config::Target::NodeApiserver) {
        return Err(skip_test("inflight metrics are a nodeapiserver-only check"));
    }

    let endpoint = format!("{}/metrics", cfg.apiserver_server().trim_end_matches('/'));
    let output = Command::new("curl")
        .args(["-k", "-sS", "--max-time", "10", &endpoint])
        .output()
        .context("reading nodeapiserver inflight metrics")?;
    anyhow::ensure!(
        output.status.success(),
        "nodeapiserver metrics request failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let metrics = String::from_utf8_lossy(&output.stdout);
    anyhow::ensure!(
        metrics.contains("# TYPE apiserver_current_inflight_requests gauge")
            && metrics.contains("apiserver_current_inflight_requests{request_kind=\"mutating\"}")
            && metrics.contains("apiserver_current_inflight_requests{request_kind=\"readOnly\"}"),
        "nodeapiserver did not expose both inflight request kinds: {metrics}"
    );
    Ok(())
}

pub(super) async fn nodeapiserver_exposes_full_request_metrics(context: &E2eContext) -> Result<()> {
    let cfg = crate::config::Config::from_env()?;
    if !matches!(cfg.target, crate::config::Target::NodeApiserver) {
        return Err(skip_test("request metric labels are a nodeapiserver-only check"));
    }

    let configmaps: Api<ConfigMap> = Api::namespaced(context.client.clone(), &context.namespace);
    configmaps
        .list(&ListParams::default())
        .await
        .context("creating a namespaced list request for the nodeapiserver metrics check")?;

    let endpoint = format!("{}/metrics", cfg.apiserver_server().trim_end_matches('/'));
    let output = Command::new("curl")
        .args(["-k", "-sS", "--max-time", "10", &endpoint])
        .output()
        .context("reading nodeapiserver request metrics")?;
    anyhow::ensure!(
        output.status.success(),
        "nodeapiserver metrics request failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let metrics = String::from_utf8_lossy(&output.stdout);
    let labels = "verb=\"LIST\",dry_run=\"\",group=\"\",version=\"v1\",resource=\"configmaps\",subresource=\"\",scope=\"namespace\",component=\"apiserver\",code=\"200\"";
    anyhow::ensure!(
        metrics.contains(&format!("apiserver_request_total{{{labels}}}")),
        "nodeapiserver request metrics omitted the upstream label set: {metrics}"
    );
    anyhow::ensure!(
        metrics.contains("apiserver_request_duration_seconds_count{verb=\"LIST\",dry_run=\"\",group=\"\",version=\"v1\",resource=\"configmaps\",subresource=\"\",scope=\"namespace\",component=\"apiserver\"}"),
        "nodeapiserver duration metrics omitted the upstream label set: {metrics}"
    );
    Ok(())
}

pub(super) async fn nodeapiserver_honors_patch_dry_run(context: &E2eContext) -> Result<()> {
    let cfg = crate::config::Config::from_env()?;
    if !matches!(cfg.target, crate::config::Target::NodeApiserver) {
        return Err(skip_test("dry-run checks are only exercised against nodeapiserver"));
    }

    let name = format!("nodeapiserver-dry-run-{}", std::process::id());
    let configmaps: Api<ConfigMap> = Api::namespaced(context.client.clone(), &context.namespace);
    configmaps
        .create(
            &PostParams::default(),
            &ConfigMap {
                metadata: kube::core::ObjectMeta {
                    name: Some(name.clone()),
                    ..Default::default()
                },
                ..Default::default()
            },
        )
        .await
        .context("creating the dry-run probe ConfigMap")?;

    let uri = format!(
        "/api/v1/namespaces/{}/configmaps/{name}?dryRun=All",
        context.namespace
    );
    let response = context
        .client
        .request::<Value>(
            Request::builder()
                .method("PATCH")
                .uri(uri)
                .header("Content-Type", "application/merge-patch+json")
                .body(serde_json::to_vec(&json!({"data": {"dry-run": "yes"}}))?)?,
        )
        .await
        .context("dry-running a ConfigMap patch")?;
    anyhow::ensure!(
        response.pointer("/data/dry-run").and_then(Value::as_str) == Some("yes"),
        "nodeapiserver did not return the dry-run patch candidate: {response}"
    );

    let stored = configmaps
        .get(&name)
        .await
        .context("reading the ConfigMap after a dry-run patch")?;
    anyhow::ensure!(
        stored.data.is_none(),
        "nodeapiserver persisted a dry-run patch: {:?}",
        stored.data
    );
    configmaps
        .delete(&name, &DeleteParams::default())
        .await
        .context("deleting the dry-run probe ConfigMap")?;
    Ok(())
}

pub(super) async fn nodeapiserver_authorizes_before_special_handlers(
    context: &E2eContext,
) -> Result<()> {
    let cfg = crate::config::Config::from_env()?;
    if !matches!(cfg.target, crate::config::Target::NodeApiserver) {
        return Err(skip_test(
            "authorization-order checks are only exercised against nodeapiserver",
        ));
    }

    let _override = NodeapiserverAuthenticationOverride::install_rbac()?;
    context
        .wait_until(
            "nodeapiserver to become active after RBAC configuration",
            Duration::from_secs(60),
            || async {
                Ok(Command::new("systemctl")
                    .args(["is-active", "--quiet", "nodeapiserver.service"])
                    .status()
                    .is_ok_and(|status| status.success()))
            },
        )
        .await?;

    let url = format!(
        "https://127.0.0.1:6443/api/v1/namespaces/{}/pods/does-not-exist/status",
        context.namespace
    );
    context
        .wait_until(
            "nodeapiserver to reject an unauthorized status PATCH before the special handler",
            Duration::from_secs(60),
            || {
                let url = url.clone();
                async move {
                    let output = Command::new("curl")
                        .args([
                            "-k",
                            "-sS",
                            "--max-time",
                            "10",
                            "-o",
                            "/dev/null",
                            "-w",
                            "%{http_code}",
                            "-X",
                            "PATCH",
                            "-H",
                            "Authorization: Bearer nodeapiserver-e2e-denied",
                            "-H",
                            "Content-Type: application/merge-patch+json",
                            "-d",
                            "{}",
                            &url,
                        ])
                        .output()
                        .context("checking authorization before the status handler")?;
                    Ok(output.status.success()
                        && String::from_utf8_lossy(&output.stdout).trim() == "403")
                }
            },
        )
        .await?;
    Ok(())
}

pub(super) async fn nodeapiserver_writes_audit_log(context: &E2eContext) -> Result<()> {
    let cfg = crate::config::Config::from_env()?;
    if !matches!(cfg.target, crate::config::Target::NodeApiserver) {
        return Err(skip_test("audit-log checks are only exercised against nodeapiserver"));
    }

    let audit = NodeapiserverAuditLogOverride::install()?;
    context
        .wait_until(
            "nodeapiserver to answer requests after audit configuration",
            Duration::from_secs(60),
            || async {
                let output = Command::new("curl")
                    .args([
                        "-k",
                        "-sS",
                        "--max-time",
                        "2",
                        "-o",
                        "/dev/null",
                        "-w",
                        "%{http_code}",
                        "https://127.0.0.1:6443/healthz",
                    ])
                    .output();
                Ok(output.is_ok_and(|output| {
                    output.status.success() && String::from_utf8_lossy(&output.stdout).trim() != "000"
                }))
            },
        )
        .await?;
    let response = Command::new("curl")
        .args(["-k", "-sS", "--max-time", "10", "https://127.0.0.1:6443/version"])
        .output()
        .context("calling nodeapiserver while audit logging is enabled")?;
    anyhow::ensure!(
        response.status.success(),
        "nodeapiserver request failed: {}",
        String::from_utf8_lossy(&response.stderr)
    );

    context
        .wait_until(
            "audit log to contain the version request",
            Duration::from_secs(30),
            || async {
                Ok(fs::read_to_string(&audit.audit_log).is_ok_and(|content| {
                    content
                        .lines()
                        .any(|line| line.contains("\"requestURI\":\"/version\""))
                }))
            },
        )
        .await
}

pub(super) async fn nodeapiserver_audits_rejected_requests(context: &E2eContext) -> Result<()> {
    let cfg = crate::config::Config::from_env()?;
    if !matches!(cfg.target, crate::config::Target::NodeApiserver) {
        return Err(skip_test(
            "audit rejection checks are only exercised against nodeapiserver",
        ));
    }

    let _auth = NodeapiserverAuthenticationOverride::install()?;
    let audit = NodeapiserverAuditLogOverride::install()?;
    context
        .wait_until(
            "nodeapiserver to reject an unauthenticated request after audit configuration",
            Duration::from_secs(60),
            || async {
                let output = Command::new("curl")
                    .args([
                        "-k",
                        "-sS",
                        "--max-time",
                        "2",
                        "-o",
                        "/dev/null",
                        "-w",
                        "%{http_code}",
                        "https://127.0.0.1:6443/healthz",
                    ])
                .output();
                Ok(output.is_ok_and(|output| {
                    output.status.success()
                        && String::from_utf8_lossy(&output.stdout).trim() == "401"
                }))
            },
        )
        .await?;

    context
        .wait_until(
            "audit log to contain the rejected unauthenticated request",
            Duration::from_secs(30),
            || async {
                Ok(fs::read_to_string(&audit.audit_log).is_ok_and(|content| {
                    content.lines().any(|line| {
                        line.contains("\"requestURI\":\"/healthz\"")
                            && line.contains("\"code\":401")
                    })
                }))
            },
        )
        .await
}

pub(super) async fn nodeapiserver_rotates_audit_log(context: &E2eContext) -> Result<()> {
    let cfg = crate::config::Config::from_env()?;
    if !matches!(cfg.target, crate::config::Target::NodeApiserver) {
        return Err(skip_test(
            "audit-log rotation checks are only exercised against nodeapiserver",
        ));
    }

    let audit = NodeapiserverAuditLogOverride::install_with_rotation(Some(512), 2)?;
    context
        .wait_until(
            "nodeapiserver to answer requests after audit rotation configuration",
            Duration::from_secs(60),
            || async {
                let output = Command::new("curl")
                    .args([
                        "-k",
                        "-sS",
                        "--max-time",
                        "2",
                        "-o",
                        "/dev/null",
                        "-w",
                        "%{http_code}",
                        "https://127.0.0.1:6443/healthz",
                    ])
                    .output();
                Ok(output.is_ok_and(|output| {
                    output.status.success()
                        && String::from_utf8_lossy(&output.stdout).trim() != "000"
                }))
            },
        )
        .await?;

    for _ in 0..8 {
        let response = Command::new("curl")
            .args([
                "-k",
                "-sS",
                "--max-time",
                "10",
                "https://127.0.0.1:6443/version",
            ])
            .output()
            .context("calling nodeapiserver while audit rotation is enabled")?;
        anyhow::ensure!(
            response.status.success(),
            "nodeapiserver request failed: {}",
            String::from_utf8_lossy(&response.stderr)
        );
    }

    context
        .wait_until(
            "audit log to rotate into a numbered backup",
            Duration::from_secs(30),
            || async {
                let backup = PathBuf::from(format!("{}.1", audit.audit_log.display()));
                Ok(backup.is_file()
                    && fs::metadata(&backup).is_ok_and(|metadata| metadata.len() > 0)
                    && fs::metadata(&audit.audit_log)
                        .is_ok_and(|metadata| metadata.len() > 0))
            },
        )
        .await
}

pub(super) async fn nodeapiserver_delivers_audit_webhook(context: &E2eContext) -> Result<()> {
    let cfg = crate::config::Config::from_env()?;
    if !matches!(cfg.target, crate::config::Target::NodeApiserver) {
        return Err(skip_test(
            "audit-webhook checks are only exercised against nodeapiserver",
        ));
    }

    let listener = TcpListener::bind(("127.0.0.1", 0))
        .context("binding the e2e audit webhook")?;
    listener.set_nonblocking(true)?;
    let address = listener.local_addr()?;
    let batches = Arc::new(Mutex::new(Vec::<Value>::new()));
    let stopping = Arc::new(AtomicBool::new(false));
    let server_batches = batches.clone();
    let server_stopping = stopping.clone();
    let server = thread::spawn(move || {
        while !server_stopping.load(Ordering::Relaxed) {
            match listener.accept() {
                Ok((stream, _)) => {
                    let _ = serve_audit_webhook_connection(stream, &server_batches);
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(5));
                }
                Err(_) => break,
            }
        }
    });

    let webhook = match NodeapiserverAuditWebhookOverride::install(&format!("http://{address}/audit")) {
        Ok(webhook) => webhook,
        Err(error) => {
            stopping.store(true, Ordering::Relaxed);
            let _ = server.join();
            return Err(error);
        }
    };
    let result = async {
        context
            .wait_until(
                "nodeapiserver to answer requests after audit webhook configuration",
                Duration::from_secs(60),
                || async {
                    let output = Command::new("curl")
                        .args([
                            "-k",
                            "-sS",
                            "--max-time",
                            "2",
                            "-o",
                            "/dev/null",
                            "-w",
                            "%{http_code}",
                            "https://127.0.0.1:6443/healthz",
                        ])
                        .output();
                    Ok(output.is_ok_and(|output| {
                        output.status.success()
                            && String::from_utf8_lossy(&output.stdout).trim() != "000"
                    }))
                },
            )
            .await?;

        let response = Command::new("curl")
            .args(["-k", "-sS", "--max-time", "10", "https://127.0.0.1:6443/version"])
            .output()
            .context("calling nodeapiserver while audit webhook is enabled")?;
        anyhow::ensure!(
            response.status.success(),
            "nodeapiserver request failed: {}",
            String::from_utf8_lossy(&response.stderr)
        );

        context
            .wait_until(
                "audit webhook to receive an EventList containing the version request",
                Duration::from_secs(45),
                || {
                    let batches = batches.clone();
                    async move {
                        Ok(batches.lock().is_ok_and(|batches| {
                            batches.iter().any(|batch| {
                                batch["kind"] == "EventList"
                                    && batch["apiVersion"] == "audit.k8s.io/v1"
                                    && batch["items"].as_array().is_some_and(|items| {
                                        items.iter().any(|event| {
                                            event["requestURI"] == "/version"
                                                && event["stage"] == "ResponseComplete"
                                        })
                                    })
                            })
                        }))
                    }
                },
            )
            .await
    }
    .await;

    drop(webhook);
    stopping.store(true, Ordering::Relaxed);
    let _ = server.join();
    result
}

pub(super) async fn nodeapiserver_audits_request_and_response_objects(context: &E2eContext) -> Result<()> {
    let cfg = crate::config::Config::from_env()?;
    if !matches!(cfg.target, crate::config::Target::NodeApiserver) {
        return Err(skip_test(
            "audit object checks are only exercised against nodeapiserver",
        ));
    }

    let listener = TcpListener::bind(("127.0.0.1", 0)).context("binding the e2e audit webhook")?;
    listener.set_nonblocking(true)?;
    let address = listener.local_addr()?;
    let batches = Arc::new(Mutex::new(Vec::<Value>::new()));
    let stopping = Arc::new(AtomicBool::new(false));
    let server_batches = batches.clone();
    let server_stopping = stopping.clone();
    let server = thread::spawn(move || {
        while !server_stopping.load(Ordering::Relaxed) {
            match listener.accept() {
                Ok((stream, _)) => {
                    let _ = serve_audit_webhook_connection(stream, &server_batches);
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(5));
                }
                Err(_) => break,
            }
        }
    });

    let policy = "apiVersion: audit.k8s.io/v1\nkind: Policy\nrules:\n- level: RequestResponse\n  resources:\n  - group: \"\"\n    resources: [configmaps]\n";
    let webhook = match NodeapiserverAuditWebhookOverride::install_with_policy(
        &format!("http://{address}/audit"),
        Some(policy),
    ) {
        Ok(webhook) => webhook,
        Err(error) => {
            stopping.store(true, Ordering::Relaxed);
            let _ = server.join();
            return Err(error);
        }
    };
    let generate_name = format!("nodeapiserver-audit-body-{}-", std::process::id());
    let result = async {
        context
            .wait_until(
                "nodeapiserver to answer requests after audit object configuration",
                Duration::from_secs(60),
                || async {
                    let output = Command::new("curl")
                        .args([
                            "-k",
                            "-sS",
                            "--max-time",
                            "2",
                            "-o",
                            "/dev/null",
                            "-w",
                            "%{http_code}",
                            "https://127.0.0.1:6443/healthz",
                        ])
                        .output();
                    Ok(output.is_ok_and(|output| output.status.success() && String::from_utf8_lossy(&output.stdout).trim() != "000"))
                },
            )
            .await?;

        let configmaps: Api<ConfigMap> = Api::namespaced(context.client.clone(), "default");
        let configmap = ConfigMap {
            metadata: kube::api::ObjectMeta {
                generate_name: Some(generate_name.clone()),
                ..Default::default()
            },
            data: Some(BTreeMap::from([(String::from("audit"), String::from("request"))])),
            ..Default::default()
        };
        let created = configmaps
            .create(&PostParams::default(), &configmap)
            .await
            .context("creating a ConfigMap for audit object capture")?;
        let name = created
            .metadata
            .name
            .as_deref()
            .context("created ConfigMap had no name")?
            .to_string();

        context
            .wait_until(
                "audit webhook to receive request and response objects",
                Duration::from_secs(45),
                || {
                    let batches = batches.clone();
                    let generate_name = generate_name.clone();
                    async move {
                        Ok(batches.lock().is_ok_and(|batches| {
                            batches.iter().any(|batch| {
                                batch["kind"] == "EventList"
                                    && batch["items"].as_array().is_some_and(|items| {
                                        items.iter().any(|event| {
                                            event["level"] == "RequestResponse"
                                                && event["stage"] == "ResponseComplete"
                                                && event["requestObject"]["metadata"]["generateName"] == generate_name
                                                && event["responseObject"]["kind"] == "ConfigMap"
                                        })
                                    })
                            })
                        }))
                    }
                },
            )
            .await?;

        let _ = configmaps.delete(&name, &DeleteParams::default()).await;
        Ok::<(), anyhow::Error>(())
    }
    .await;

    drop(webhook);
    stopping.store(true, Ordering::Relaxed);
    let _ = server.join();
    result
}

fn serve_audit_webhook_connection(
    mut stream: std::net::TcpStream,
    batches: &Mutex<Vec<Value>>,
) -> std::io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    let mut request = Vec::new();
    let mut buffer = [0_u8; 4096];
    let content_length = loop {
        let read = stream.read(&mut buffer)?;
        if read == 0 {
            return Ok(());
        }
        request.extend_from_slice(&buffer[..read]);
        let Some(headers_end) = request
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
        else {
            continue;
        };
        let headers = String::from_utf8_lossy(&request[..headers_end]);
        break headers
            .lines()
            .find_map(|line| {
                line.strip_prefix("Content-Length:")
                    .or_else(|| line.strip_prefix("content-length:"))
            })
            .and_then(|value| value.trim().parse::<usize>().ok())
            .unwrap_or(0);
    };
    let headers_end = request
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .expect("HTTP headers were found above")
        + 4;
    while request.len() < headers_end + content_length {
        let read = stream.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        request.extend_from_slice(&buffer[..read]);
    }
    let payload: Value = serde_json::from_slice(&request[headers_end..headers_end + content_length])
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
    batches
        .lock()
        .map_err(|_| std::io::Error::other("audit webhook batch list was poisoned"))?
        .push(payload);
    stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
}

pub(super) async fn nodeapiserver_rejects_unsupported_field_selector(context: &E2eContext) -> Result<()> {
    let cfg = crate::config::Config::from_env()?;
    if !matches!(cfg.target, crate::config::Target::NodeApiserver) {
        return Err(skip_test("selectable field checks are only exercised against nodeapiserver"));
    }

    let configmaps: Api<ConfigMap> = Api::all(context.client.clone());
    match configmaps
        .list(&ListParams::default().fields("data.unsupported=value"))
        .await
    {
        Err(KubeError::Api(error)) => {
            if error.code != 400 {
                anyhow::bail!(
                    "nodeapiserver returned the wrong status for an unsupported field selector: {error}"
                );
            }
        }
        Err(error) => anyhow::bail!(
            "nodeapiserver returned a non-API error for an unsupported field selector: {error}"
        ),
        Ok(list) => anyhow::bail!(
            "nodeapiserver accepted an unsupported field selector and returned {} ConfigMaps",
            list.items.len()
        ),
    }
    Ok(())
}

pub(super) async fn nodeapiserver_serves_generic_status_subresource(context: &E2eContext) -> Result<()> {
    let cfg = crate::config::Config::from_env()?;
    if !matches!(cfg.target, crate::config::Target::NodeApiserver) {
        return Err(skip_test("status-subresource checks are only exercised against nodeapiserver"));
    }

    let name = format!("nodeapiserver-status-{}", std::process::id());
    let configmaps: Api<ConfigMap> = Api::namespaced(context.client.clone(), &context.namespace);
    configmaps
        .create(
            &PostParams::default(),
            &ConfigMap {
                metadata: kube::core::ObjectMeta {
                    name: Some(name.clone()),
                    ..Default::default()
                },
                ..Default::default()
            },
        )
        .await
        .context("creating the status-subresource probe ConfigMap")?;

    let uri = format!(
        "/api/v1/namespaces/{}/configmaps/{name}/status",
        context.namespace
    );
    let response = context
        .client
        .request::<Value>(
            Request::builder()
                .method("PATCH")
                .uri(uri)
                .header("Content-Type", "application/merge-patch+json")
                .body(serde_json::to_vec(&json!({"status": {"phase": "Ready"}}))?)?,
        )
        .await
        .context("patching a generic resource status subresource")?;
    anyhow::ensure!(
        response.pointer("/status/phase").and_then(Value::as_str) == Some("Ready"),
        "nodeapiserver did not persist the status-only patch: {response}"
    );
    configmaps
        .delete(&name, &DeleteParams::default())
        .await
        .context("deleting the status-subresource probe ConfigMap")?;
    Ok(())
}

pub(super) async fn nodeapiserver_excludes_status_from_main_managed_fields(
    context: &E2eContext,
) -> Result<()> {
    let cfg = crate::config::Config::from_env()?;
    if !matches!(cfg.target, crate::config::Target::NodeApiserver) {
        return Err(skip_test(
            "managed-field exclusion checks are only exercised against nodeapiserver",
        ));
    }

    let name = format!("nodeapiserver-managed-status-{}", std::process::id());
    let base_uri = format!("/api/v1/namespaces/{}/configmaps/{name}", context.namespace);
    let configmaps: Api<ConfigMap> = Api::namespaced(context.client.clone(), &context.namespace);
    configmaps
        .create(
            &PostParams::default(),
            &ConfigMap {
                metadata: kube::core::ObjectMeta {
                    name: Some(name.clone()),
                    ..Default::default()
                },
                ..Default::default()
            },
        )
        .await
        .context("creating the managed-field status fixture")?;

    let result = async {
        let apply: Value = context
            .client
            .request(
                Request::builder()
                    .method("PATCH")
                    .uri(format!("{base_uri}?fieldManager=nodeapiserver-main"))
                    .header("Content-Type", "application/apply-patch+yaml")
                    .body(serde_json::to_vec(&json!({
                        "apiVersion": "v1",
                        "kind": "ConfigMap",
                        "metadata": {"name": name, "namespace": context.namespace},
                        "data": {"value": "one"},
                        "status": {"phase": "ignored-by-main-resource"}
                    }))?)?,
            )
            .await
            .context("applying a main ConfigMap resource containing status")?;
        let main_entry = apply
            .pointer("/metadata/managedFields")
            .and_then(Value::as_array)
            .and_then(|entries| {
                entries.iter().find(|entry| {
                    entry.get("manager").and_then(Value::as_str) == Some("nodeapiserver-main")
                        && entry
                            .get("subresource")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .is_empty()
                })
            })
            .context("main Apply response had no managed-fields entry")?;
        anyhow::ensure!(
            main_entry.pointer("/fieldsV1/f:status").is_none(),
            "main-resource Apply incorrectly claimed the server-managed status field: {apply}"
        );
        anyhow::ensure!(
            main_entry.pointer("/fieldsV1/f:data").is_some(),
            "main-resource Apply did not retain ownership of data: {apply}"
        );

        let status: Value = context
            .client
            .request(
                Request::builder()
                    .method("PATCH")
                    .uri(format!("{base_uri}/status?fieldManager=nodeapiserver-status"))
                    .header("Content-Type", "application/merge-patch+json")
                    .body(serde_json::to_vec(&json!({"status": {"phase": "ready"}}))?)?,
            )
            .await
            .context("updating the ConfigMap status subresource")?;
        let status_entry = status
            .pointer("/metadata/managedFields")
            .and_then(Value::as_array)
            .and_then(|entries| {
                entries.iter().find(|entry| {
                    entry.get("manager").and_then(Value::as_str) == Some("nodeapiserver-status")
                        && entry.get("subresource").and_then(Value::as_str) == Some("status")
                })
            })
            .context("status update response had no status managed-fields entry")?;
        anyhow::ensure!(
            status_entry.pointer("/fieldsV1/f:status").is_some(),
            "status subresource did not claim status ownership: {status}"
        );
        Ok::<(), anyhow::Error>(())
    }
    .await;

    let _ = context
        .client
        .request::<Value>(Request::builder().method("DELETE").uri(base_uri).body(Vec::new())?)
        .await;
    result
}

pub(super) async fn nodeapiserver_serves_ephemeralcontainers_subresource(
    context: &E2eContext,
) -> Result<()> {
    let cfg = crate::config::Config::from_env()?;
    if !matches!(cfg.target, crate::config::Target::NodeApiserver) {
        return Err(skip_test(
            "ephemeralcontainers checks are only exercised against nodeapiserver",
        ));
    }

    let name = format!("nodeapiserver-ephemeral-{}", std::process::id());
    let pods: Api<Pod> = Api::namespaced(context.client.clone(), &context.namespace);
    let pod: Pod = serde_json::from_value(json!({
        "apiVersion": "v1",
        "kind": "Pod",
        "metadata": {"name": name},
        "spec": {
            "containers": [{
                "name": "app",
                "image": "busybox:latest",
                "command": ["sleep", "3600"]
            }]
        }
    }))?;
    pods.create(&PostParams::default(), &pod)
        .await
        .context("creating the nodeapiserver ephemeralcontainers fixture")?;
    context
        .wait_until(
            "nodeapiserver ephemeralcontainers fixture to become Running",
            Duration::from_secs(90),
            || {
                let pods = pods.clone();
                let name = name.clone();
                async move {
                    Ok(pods
                        .get(&name)
                        .await?
                        .status
                        .and_then(|status| status.phase)
                        .as_deref()
                        == Some("Running"))
                }
            },
        )
        .await?;

    let response = context
        .client
        .request::<Value>(
            Request::builder()
                .method("PATCH")
                .uri(format!(
                    "/api/v1/namespaces/{}/pods/{name}/ephemeralcontainers",
                    context.namespace
                ))
                .header("Content-Type", "application/strategic-merge-patch+json")
                .body(serde_json::to_vec(&json!({
                    "metadata": {"labels": {"must-be-reset": "true"}},
                    "spec": {"ephemeralContainers": [{
                        "name": "debugger",
                        "image": "busybox:latest",
                        "command": ["sleep", "3600"],
                        "targetContainerName": "app"
                    }]}
                }))?)?,
        )
        .await
        .context("patching nodeapiserver ephemeralcontainers")?;
    anyhow::ensure!(
        response.pointer("/spec/ephemeralContainers/0/name").and_then(Value::as_str)
            == Some("debugger"),
        "nodeapiserver did not return the appended ephemeral container: {response}"
    );

    let persisted = pods
        .get(&name)
        .await
        .context("reading the Pod after the ephemeralcontainers patch")?;
    anyhow::ensure!(
        persisted
            .metadata
            .labels
            .as_ref()
            .is_none_or(|labels| !labels.contains_key("must-be-reset")),
        "ephemeralcontainers accepted an unrelated metadata mutation"
    );
    anyhow::ensure!(
        persisted
            .spec
            .as_ref()
            .and_then(|spec| spec.ephemeral_containers.as_ref())
            .is_some_and(|containers| containers.iter().any(|container| container.name == "debugger")),
        "nodeapiserver did not persist the appended ephemeral container"
    );
    pods.delete(&name, &DeleteParams::default())
        .await
        .context("deleting the nodeapiserver ephemeralcontainers fixture")?;
    Ok(())
}

pub(super) async fn nodeapiserver_enforces_service_account_mountable_secrets(
    context: &E2eContext,
) -> Result<()> {
    let cfg = crate::config::Config::from_env()?;
    if !matches!(cfg.target, crate::config::Target::NodeApiserver) {
        return Err(skip_test(
            "mountable ServiceAccount secret checks are only exercised against nodeapiserver",
        ));
    }

    let suffix = std::process::id();
    let service_account_name = format!("nodeapiserver-mountable-{suffix}");
    let secret_name = format!("nodeapiserver-mountable-secret-{suffix}");
    let denied_pod_name = format!("nodeapiserver-mountable-denied-{suffix}");
    let allowed_pod_name = format!("nodeapiserver-mountable-allowed-{suffix}");
    let secrets: Api<Secret> = Api::namespaced(context.client.clone(), &context.namespace);
    let service_accounts: Api<ServiceAccount> =
        Api::namespaced(context.client.clone(), &context.namespace);
    let pods: Api<Pod> = Api::namespaced(context.client.clone(), &context.namespace);

    secrets
        .create(
            &PostParams::default(),
            &Secret {
                metadata: kube::core::ObjectMeta {
                    name: Some(secret_name.clone()),
                    ..Default::default()
                },
                string_data: Some(BTreeMap::from([(String::from("token"), String::from("allowed"))])),
                ..Default::default()
            },
        )
        .await
        .context("creating the mountable-secret fixture")?;
    service_accounts
        .create(
            &PostParams::default(),
            &ServiceAccount {
                metadata: kube::core::ObjectMeta {
                    name: Some(service_account_name.clone()),
                    annotations: Some(BTreeMap::from([(
                        String::from("kubernetes.io/enforce-mountable-secrets"),
                        String::from("true"),
                    )])),
                    ..Default::default()
                },
                secrets: Some(vec![ObjectReference {
                    name: Some(secret_name.clone()),
                    ..Default::default()
                }]),
                image_pull_secrets: Some(vec![LocalObjectReference {
                    name: secret_name.clone(),
                }]),
                ..Default::default()
            },
        )
        .await
        .context("creating the mountable-secret ServiceAccount fixture")?;

    let denied: Pod = serde_json::from_value(json!({
        "apiVersion": "v1",
        "kind": "Pod",
        "metadata": {"name": denied_pod_name},
        "spec": {
            "serviceAccountName": service_account_name,
            "volumes": [{"name": "credentials", "secret": {"secretName": "not-listed"}}],
            "containers": [{"name": "app", "image": "busybox:latest", "command": ["sleep", "3600"]}]
        }
    }))?;
    match pods.create(&PostParams::default(), &denied).await {
        Err(KubeError::Api(error)) if error.code == 422 => {}
        Err(error) => anyhow::bail!(
            "an unlisted mountable secret returned the wrong API error: {error}"
        ),
        Ok(pod) => anyhow::bail!(
            "an unlisted mountable secret was unexpectedly admitted: {:?}",
            pod.metadata.name
        ),
    }

    let allowed: Pod = serde_json::from_value(json!({
        "apiVersion": "v1",
        "kind": "Pod",
        "metadata": {"name": allowed_pod_name},
        "spec": {
            "serviceAccountName": service_account_name,
            "volumes": [{"name": "credentials", "secret": {"secretName": secret_name}}],
            "imagePullSecrets": [{"name": secret_name}],
            "containers": [{"name": "app", "image": "busybox:latest", "command": ["sleep", "3600"]}]
        }
    }))?;
    pods.create(&PostParams::default(), &allowed)
        .await
        .context("creating a Pod with only mountable ServiceAccount secrets")?;
    context
        .wait_until("the mountable-secret allow fixture to become Running", Duration::from_secs(90), || {
            let pods = pods.clone();
            let name = allowed.metadata.name.clone().unwrap_or_default();
            async move {
                Ok(pods
                    .get(&name)
                    .await?
                    .status
                    .and_then(|status| status.phase)
                    .as_deref()
                    == Some("Running"))
            }
        })
        .await?;
    pods.delete(&allowed.metadata.name.clone().unwrap_or_default(), &DeleteParams::default())
        .await
        .context("deleting the mountable-secret allow fixture")?;
    Ok(())
}

pub(super) async fn nodeapiserver_enforces_mountable_secrets_for_ephemeral_containers(
    context: &E2eContext,
) -> Result<()> {
    let cfg = crate::config::Config::from_env()?;
    if !matches!(cfg.target, crate::config::Target::NodeApiserver) {
        return Err(skip_test(
            "ephemeral-container ServiceAccount secret checks are only exercised against nodeapiserver",
        ));
    }

    let suffix = std::process::id();
    let service_account_name = format!("nodeapiserver-ephemeral-sa-{suffix}");
    let secret_name = format!("nodeapiserver-ephemeral-secret-{suffix}");
    let pod_name = format!("nodeapiserver-ephemeral-secret-pod-{suffix}");
    let secrets: Api<Secret> = Api::namespaced(context.client.clone(), &context.namespace);
    let service_accounts: Api<ServiceAccount> =
        Api::namespaced(context.client.clone(), &context.namespace);
    let pods: Api<Pod> = Api::namespaced(context.client.clone(), &context.namespace);

    secrets
        .create(
            &PostParams::default(),
            &Secret {
                metadata: kube::core::ObjectMeta {
                    name: Some(secret_name.clone()),
                    ..Default::default()
                },
                string_data: Some(BTreeMap::from([(
                    String::from("token"),
                    String::from("allowed"),
                )])),
                ..Default::default()
            },
        )
        .await
        .context("creating the ephemeral-container mountable-secret fixture")?;
    service_accounts
        .create(
            &PostParams::default(),
            &ServiceAccount {
                metadata: kube::core::ObjectMeta {
                    name: Some(service_account_name.clone()),
                    annotations: Some(BTreeMap::from([(
                        String::from("kubernetes.io/enforce-mountable-secrets"),
                        String::from("true"),
                    )])),
                    ..Default::default()
                },
                secrets: Some(vec![ObjectReference {
                    name: Some(secret_name.clone()),
                    ..Default::default()
                }]),
                ..Default::default()
            },
        )
        .await
        .context("creating the ephemeral-container ServiceAccount fixture")?;

    let pod: Pod = serde_json::from_value(json!({
        "apiVersion": "v1",
        "kind": "Pod",
        "metadata": {"name": pod_name},
        "spec": {
            "serviceAccountName": service_account_name,
            "containers": [{"name": "app", "image": "busybox:latest", "command": ["sleep", "3600"]}]
        }
    }))?;
    pods.create(&PostParams::default(), &pod)
        .await
        .context("creating the ephemeral-container secret fixture Pod")?;
    context
        .wait_until(
            "the ephemeral-container secret fixture Pod to become Running",
            Duration::from_secs(90),
            || {
                let pods = pods.clone();
                let name = pod_name.clone();
                async move {
                    Ok(pods
                        .get(&name)
                        .await?
                        .status
                        .and_then(|status| status.phase)
                        .as_deref()
                        == Some("Running"))
                }
            },
        )
        .await?;

    let ephemeral_uri = format!(
        "/api/v1/namespaces/{}/pods/{pod_name}/ephemeralcontainers",
        context.namespace
    );
    let denied_request = Request::builder()
        .method("PATCH")
        .uri(ephemeral_uri.as_str())
        .header("Content-Type", "application/strategic-merge-patch+json")
        .body(serde_json::to_vec(&json!({
            "spec": {"ephemeralContainers": [{
                "name": "denied-debugger",
                "image": "busybox:latest",
                "command": ["sleep", "3600"],
                "env": [{"name": "TOKEN", "valueFrom": {"secretKeyRef": {"name": "not-listed", "key": "token"}}}]
            }]}
        }))?)?;
    let denied = context.client.request::<Value>(denied_request).await;
    match denied {
        Err(KubeError::Api(error)) if error.code == 422 => {}
        Err(error) => anyhow::bail!(
            "an unlisted ephemeral-container secret returned the wrong API error: {error}"
        ),
        Ok(response) => anyhow::bail!(
            "an unlisted ephemeral-container secret was unexpectedly admitted: {response}"
        ),
    }

    let allowed_request = Request::builder()
        .method("PATCH")
        .uri(ephemeral_uri.as_str())
        .header("Content-Type", "application/strategic-merge-patch+json")
        .body(serde_json::to_vec(&json!({
            "spec": {"ephemeralContainers": [{
                "name": "allowed-debugger",
                "image": "busybox:latest",
                "command": ["sleep", "3600"],
                "env": [{"name": "TOKEN", "valueFrom": {"secretKeyRef": {"name": secret_name, "key": "token"}}}]
            }]}
        }))?)?;
    let allowed = context
        .client
        .request::<Value>(allowed_request)
        .await
        .context("adding an allowed ephemeral container")?;
    anyhow::ensure!(
        allowed
            .pointer("/spec/ephemeralContainers/0/name")
            .and_then(Value::as_str)
            == Some("allowed-debugger"),
        "nodeapiserver did not admit the allowed ephemeral-container secret reference: {allowed}"
    );

    pods.delete(&pod_name, &DeleteParams::default())
        .await
        .context("deleting the ephemeral-container secret fixture Pod")?;
    service_accounts
        .delete(&service_account_name, &DeleteParams::default())
        .await
        .context("deleting the ephemeral-container ServiceAccount fixture")?;
    secrets
        .delete(&secret_name, &DeleteParams::default())
        .await
        .context("deleting the ephemeral-container mountable-secret fixture")?;
    Ok(())
}

pub(super) async fn nodeapiserver_watches_an_uncommon_builtin_resource(context: &E2eContext) -> Result<()> {
    let cfg = crate::config::Config::from_env()?;
    if !matches!(cfg.target, crate::config::Target::NodeApiserver) {
        return Err(skip_test("built-in watch-cache coverage is a nodeapiserver-only check"));
    }

    let name = format!("nodeapiserver-cache-{}", std::process::id());
    let priority_classes: Api<PriorityClass> = Api::all(context.client.clone());
    let watch = priority_classes.watch(&WatchParams::default().timeout(30), "0").await?;
    futures::pin_mut!(watch);
    let result = async {
        let priority_class: PriorityClass = serde_json::from_value(json!({
            "apiVersion": "scheduling.k8s.io/v1",
            "kind": "PriorityClass",
            "metadata": {"name": name.clone()},
            "value": 123456,
            "description": "nodeapiserver built-in cache e2e"
        }))?;
        priority_classes
            .create(&PostParams::default(), &priority_class)
            .await
            .context("creating the uncommon built-in watch-cache fixture")?;

        let deadline = Instant::now() + Duration::from_secs(30);
        while Instant::now() < deadline {
            let event = tokio::time::timeout(Duration::from_secs(1), watch.next()).await;
            let Ok(Some(event)) = event else { continue };
            match event? {
                WatchEvent::Added(object) | WatchEvent::Modified(object) if object.metadata.name.as_deref() == Some(name.as_str()) => return Ok(()),
                WatchEvent::Error(status) => anyhow::bail!("built-in resource watch returned an error: {status:?}"),
                _ => {}
            }
        }
        anyhow::bail!("nodeapiserver did not deliver a watch event for the uncommon built-in resource")
    }
    .await;
    let _ = priority_classes.delete(&name, &DeleteParams::default()).await;
    result
}

pub(super) async fn nodeapiserver_honors_watch_options(context: &E2eContext) -> Result<()> {
    let cfg = crate::config::Config::from_env()?;
    if !matches!(cfg.target, crate::config::Target::NodeApiserver) {
        return Err(skip_test("watch option compatibility is a nodeapiserver-only check"));
    }

    let name = format!("nodeapiserver-watch-options-{}", std::process::id());
    let request = Request::builder()
        .method("GET")
        .uri(format!(
            "/api/v1/namespaces/{}/configmaps?watch=true&resourceVersion=0&allowWatchBookmarks=false&timeoutSeconds=1&fieldSelector=metadata.name%3D{}",
            context.namespace, name
        ))
        .body(Vec::new())?;
    let mut stream = context
        .client
        .request_stream(request)
        .await
        .context("starting the watch-options compatibility check")?;
    let mut line = String::new();
    let bytes = tokio::time::timeout(Duration::from_secs(5), stream.read_line(&mut line))
        .await
        .context("watch did not honor timeoutSeconds")??;
    anyhow::ensure!(bytes == 0, "watch returned an unexpected event before timeout: {line}");
    Ok(())
}

pub(super) async fn nodeapiserver_recreates_a_dynamic_watch_cache(
    context: &E2eContext,
) -> Result<()> {
    let cfg = crate::config::Config::from_env()?;
    if !matches!(cfg.target, crate::config::Target::NodeApiserver) {
        return Err(skip_test("dynamic watch-cache lifecycle is a nodeapiserver-only check"));
    }

    let suffix = std::process::id();
    let group = format!("nodeapiserver-cache-{suffix}.test");
    let crd_name = format!("widgets.{group}");
    let crds: Api<CustomResourceDefinition> = Api::all(context.client.clone());
    let widgets: Api<DynamicObject> = Api::all_with(
        context.client.clone(),
        &ApiResource::from_gvk(&GroupVersionKind::gvk(&group, "v1", "Widget")),
    );
    let crd: CustomResourceDefinition = serde_json::from_value(json!({
        "apiVersion": "apiextensions.k8s.io/v1",
        "kind": "CustomResourceDefinition",
        "metadata": {"name": crd_name},
        "spec": {
            "group": group,
            "names": {
                "plural": "widgets",
                "singular": "widget",
                "kind": "Widget",
                "listKind": "WidgetList"
            },
            "scope": "Cluster",
            "versions": [{
                "name": "v1",
                "served": true,
                "storage": true,
                "schema": {"openAPIV3Schema": {
                    "type": "object",
                    "properties": {"spec": {"type": "object"}}
                }}
            }]
        }
    }))?;

    let first_name = format!("first-{suffix}");
    let second_name = format!("second-{suffix}");
    let result = async {
        crds.create(&PostParams::default(), &crd)
            .await
            .context("creating the dynamic watch-cache CRD")?;
        context
            .wait_until("dynamic watch-cache CRD to become established", Duration::from_secs(60), || {
                let crds = crds.clone();
                let crd_name = crd_name.clone();
                async move {
                    Ok(crds
                        .get_opt(&crd_name)
                        .await?
                        .is_some_and(|crd| crd_is_established(&crd)))
                }
            })
            .await?;

        {
            let watch = widgets.watch(&WatchParams::default().timeout(30), "0").await?;
            futures::pin_mut!(watch);
            let object: DynamicObject = serde_json::from_value(json!({
                "apiVersion": format!("{group}/v1"),
                "kind": "Widget",
                "metadata": {"name": first_name}
            }))?;
            widgets
                .create(&PostParams::default(), &object)
                .await
                .context("creating the first CRD-backed watch object")?;
            let deadline = Instant::now() + Duration::from_secs(30);
            let mut observed = false;
            while Instant::now() < deadline {
                let event = tokio::time::timeout(Duration::from_secs(1), watch.next()).await;
                let Ok(Some(event)) = event else { continue };
                match event? {
                    WatchEvent::Added(object) | WatchEvent::Modified(object)
                        if object.metadata.name.as_deref() == Some(first_name.as_str()) =>
                    {
                        observed = true;
                        break;
                    }
                    WatchEvent::Error(status) => anyhow::bail!("first dynamic watch returned an error: {status:?}"),
                    _ => {}
                }
            }
            anyhow::ensure!(observed, "nodeapiserver did not deliver the first CRD-backed watch event");
        }

        crds.delete(&crd_name, &DeleteParams::default()).await?;
        context
            .wait_until("dynamic watch-cache CRD to be deleted", Duration::from_secs(60), || {
                let crds = crds.clone();
                let crd_name = crd_name.clone();
                async move { Ok(crds.get_opt(&crd_name).await?.is_none()) }
            })
            .await?;

        crds.create(&PostParams::default(), &crd)
            .await
            .context("recreating the dynamic watch-cache CRD")?;
        context
            .wait_until("recreated dynamic watch-cache CRD to become established", Duration::from_secs(60), || {
                let crds = crds.clone();
                let crd_name = crd_name.clone();
                async move {
                    Ok(crds
                        .get_opt(&crd_name)
                        .await?
                        .is_some_and(|crd| crd_is_established(&crd)))
                }
            })
            .await?;

        let watch_deadline = Instant::now() + Duration::from_secs(30);
        let watch = loop {
            match widgets.watch(&WatchParams::default().timeout(30), "0").await {
                Ok(watch) => break watch,
                Err(KubeError::Api(error)) if error.code == 404 && Instant::now() < watch_deadline => {
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
                Err(error) => return Err(error.into()),
            }
        };
        futures::pin_mut!(watch);
        let object: DynamicObject = serde_json::from_value(json!({
            "apiVersion": format!("{group}/v1"),
            "kind": "Widget",
            "metadata": {"name": second_name}
        }))?;
        widgets
            .create(&PostParams::default(), &object)
            .await
            .context("creating the recreated CRD-backed watch object")?;
        let deadline = Instant::now() + Duration::from_secs(30);
        let mut observed = false;
        while Instant::now() < deadline {
            let event = tokio::time::timeout(Duration::from_secs(1), watch.next()).await;
            let Ok(Some(event)) = event else { continue };
            match event? {
                WatchEvent::Added(object) | WatchEvent::Modified(object)
                    if object.metadata.name.as_deref() == Some(second_name.as_str()) =>
                {
                    observed = true;
                    break;
                }
                WatchEvent::Error(status) => anyhow::bail!("recreated dynamic watch returned an error: {status:?}"),
                _ => {}
            }
        }
        anyhow::ensure!(observed, "nodeapiserver did not deliver a watch event after CRD recreation");
        Ok::<(), anyhow::Error>(())
    }
    .await;

    let _ = widgets.delete(&first_name, &DeleteParams::default()).await;
    let _ = widgets.delete(&second_name, &DeleteParams::default()).await;
    let _ = crds.delete(&crd_name, &DeleteParams::default()).await;
    result
}

pub(super) async fn nodeapiserver_rejects_unsupported_resource_route(
    context: &E2eContext,
) -> Result<()> {
    let cfg = crate::config::Config::from_env()?;
    if !matches!(cfg.target, crate::config::Target::NodeApiserver) {
        return Err(skip_test(
            "unsupported-resource route checks are only exercised against nodeapiserver",
        ));
    }

    let request = Request::builder()
        .method("GET")
        .uri(format!(
            "/api/v1/namespaces/{}/pods/nodeapiserver-route-check/unsupported",
            context.namespace
        ))
        .body(Vec::new())?;
    match context.client.request::<Value>(request).await {
        Err(KubeError::Api(error)) if error.code == 404 => {}
        Err(error) => anyhow::bail!(
            "unsupported resource route returned the wrong API error: {error}"
        ),
        Ok(value) => anyhow::bail!(
            "unsupported resource route was accepted instead of returning 404: {value}"
        ),
    }

    let request = Request::builder()
        .method("GET")
        .uri("/nodeapiserver-route-check/unsupported")
        .body(Vec::new())?;
    match context.client.request::<Value>(request).await {
        Err(KubeError::Api(error)) if error.code == 404 => Ok(()),
        Err(error) => anyhow::bail!(
            "unsupported non-resource route returned the wrong API error: {error}"
        ),
        Ok(value) => anyhow::bail!(
            "unsupported non-resource route was accepted instead of returning 404: {value}"
        ),
    }
}

pub(super) async fn nodeapiserver_rejects_oversized_request_body(
    context: &E2eContext,
) -> Result<()> {
    let cfg = crate::config::Config::from_env()?;
    if !matches!(cfg.target, crate::config::Target::NodeApiserver) {
        return Err(skip_test(
            "request-body size checks are only exercised against nodeapiserver",
        ));
    }

    let body = vec![b'x'; 3 * 1024 * 1024 + 1];
    let request = Request::builder()
        .method("POST")
        .uri(format!("/api/v1/namespaces/{}/configmaps", context.namespace))
        .header("content-type", "application/json")
        .body(body)?;
    match context.client.request::<Value>(request).await {
        Err(KubeError::Api(error)) if error.code == 413 => Ok(()),
        Err(error) => anyhow::bail!(
            "oversized request body returned the wrong API error: {error}"
        ),
        Ok(value) => anyhow::bail!(
            "oversized request body was accepted instead of returning 413: {value}"
        ),
    }
}

pub(super) async fn nodeapiserver_validating_admission_policy_denies_create(context: &E2eContext) -> Result<()> {
    let cfg = crate::config::Config::from_env()?;
    if !matches!(cfg.target, crate::config::Target::NodeApiserver) {
        return Err(skip_test("ValidatingAdmissionPolicy enforcement is a nodeapiserver-only check"));
    }

    let suffix = std::process::id();
    let policy_name = format!("nodeapiserver-vap-{suffix}");
    let binding_name = format!("nodeapiserver-vap-binding-{suffix}");
    let parameter_name = format!("nodeapiserver-vap-parameter-{suffix}");
    let configmaps: Api<ConfigMap> = Api::namespaced(context.client.clone(), "default");
    configmaps
        .create(
            &PostParams::default(),
            &ConfigMap {
                metadata: kube::api::ObjectMeta {
                    name: Some(parameter_name.clone()),
                    ..Default::default()
                },
                data: Some(BTreeMap::from([("allowed".to_string(), format!("nodeapiserver-vap-allowed-{suffix}"))])),
                ..Default::default()
            },
        )
        .await
        .context("creating the ValidatingAdmissionPolicy parameter ConfigMap")?;
    let policy = json!({
        "apiVersion": "admissionregistration.k8s.io/v1",
        "kind": "ValidatingAdmissionPolicy",
        "metadata": {"name": policy_name},
        "spec": {
            "failurePolicy": "Fail",
            "paramKind": {"apiGroup": "", "kind": "ConfigMap"},
            "matchConstraints": {
                "resourceRules": [{
                    "apiGroups": [""],
                    "apiVersions": ["v1"],
                    "operations": ["CREATE"],
                    "resources": ["configmaps"]
                }]
            },
            "validations": [{
                "expression": "params.data.allowed == object.metadata.name",
                "message": "this e2e policy only permits its named canary"
            }]
        }
    });
    let create_policy = Request::builder()
        .method("POST")
        .uri("/apis/admissionregistration.k8s.io/v1/validatingadmissionpolicies")
        .header("Content-Type", "application/json")
        .body(serde_json::to_vec(&policy)?)?;
    context.client.request::<serde_json::Value>(create_policy).await.context("creating the e2e ValidatingAdmissionPolicy")?;

    let binding = json!({
        "apiVersion": "admissionregistration.k8s.io/v1",
        "kind": "ValidatingAdmissionPolicyBinding",
        "metadata": {"name": binding_name},
        "spec": {
            "policyName": policy_name,
            "validationActions": ["Deny"],
            "paramRef": {"name": parameter_name, "parameterNotFoundAction": "Deny"}
        }
    });
    let create_binding = Request::builder()
        .method("POST")
        .uri("/apis/admissionregistration.k8s.io/v1/validatingadmissionpolicybindings")
        .header("Content-Type", "application/json")
        .body(serde_json::to_vec(&binding)?)?;
    context.client.request::<serde_json::Value>(create_binding).await.context("creating the e2e ValidatingAdmissionPolicyBinding")?;

    let denied = configmaps
        .create(
            &PostParams::default(),
            &ConfigMap {
                metadata: kube::api::ObjectMeta {
                    name: Some(format!("nodeapiserver-vap-denied-{suffix}")),
                    ..Default::default()
                },
                ..Default::default()
            },
        )
        .await;
    match denied {
        Err(KubeError::Api(error)) if error.code == 403 => {}
        Err(error) => anyhow::bail!("ValidatingAdmissionPolicy denial returned the wrong API error: {error}"),
        Ok(object) => anyhow::bail!("ValidatingAdmissionPolicy unexpectedly admitted {:?}", object.metadata.name),
    }
    let allowed_name = format!("nodeapiserver-vap-allowed-{suffix}");
    configmaps
        .create(
            &PostParams::default(),
            &ConfigMap {
                metadata: kube::api::ObjectMeta {
                    name: Some(allowed_name.clone()),
                    ..Default::default()
                },
                ..Default::default()
            },
        )
        .await
        .context("creating the parameter-approved ConfigMap")?;
    configmaps
        .delete(&allowed_name, &DeleteParams::default())
        .await
        .context("cleaning up the parameter-approved ConfigMap")?;

    for (method, uri) in [
        ("DELETE", format!("/apis/admissionregistration.k8s.io/v1/validatingadmissionpolicybindings/{binding_name}")),
        ("DELETE", format!("/apis/admissionregistration.k8s.io/v1/validatingadmissionpolicies/{policy_name}")),
    ] {
        let request = Request::builder().method(method).uri(uri).body(Vec::new())?;
        let _ = context.client.request::<serde_json::Value>(request).await;
    }
    let _ = configmaps
        .delete(&parameter_name, &DeleteParams::default())
        .await;
    Ok(())
}

pub(super) async fn nodeapiserver_enforces_crd_schema_constraints(context: &E2eContext) -> Result<()> {
    let cfg = crate::config::Config::from_env()?;
    if !matches!(cfg.target, crate::config::Target::NodeApiserver) {
        return Err(skip_test("CRD schema constraint enforcement is a nodeapiserver-only check"));
    }

    let suffix = std::process::id();
    let group = format!("nodeapiserver-e2e-{suffix}.test");
    let crd_name = format!("widgets.{group}");
    let widget_name = format!("widget-{suffix}");
    let crds: Api<CustomResourceDefinition> = Api::all(context.client.clone());
    let widgets: Api<DynamicObject> = Api::namespaced_with(
        context.client.clone(),
        &context.namespace,
        &ApiResource::from_gvk(&GroupVersionKind::gvk(&group, "v1", "Widget")),
    );
    let crd: CustomResourceDefinition = serde_json::from_value(json!({
        "apiVersion": "apiextensions.k8s.io/v1",
        "kind": "CustomResourceDefinition",
        "metadata": {"name": crd_name},
        "spec": {
            "group": group.clone(),
            "names": {
                "plural": "widgets",
                "singular": "widget",
                "kind": "Widget",
                "listKind": "WidgetList"
            },
            "scope": "Namespaced",
            "versions": [{
                "name": "v1",
                "served": true,
                "storage": true,
                "schema": {"openAPIV3Schema": {
                    "type": "object",
                    "required": ["spec"],
                    "properties": {"spec": {
                        "type": "object",
                        "required": ["color", "replicas"],
                        "properties": {
                            "color": {"type": "string", "enum": ["blue", "green"]},
                            "replicas": {"type": "integer", "minimum": 1, "maximum": 3}
                        }
                    }}
                }}
            }]
        }
    }))?;

    let result = async {
        crds.create(&PostParams::default(), &crd)
            .await
            .context("creating a CRD with enum and numeric constraints")?;
        context
            .wait_until("CRD schema constraint resource to become established", Duration::from_secs(60), || {
                let crds = crds.clone();
                let crd_name = crd_name.clone();
                async move {
                    Ok(crds
                        .get_opt(&crd_name)
                        .await?
                        .and_then(|crd| crd.status)
                        .and_then(|status| status.conditions)
                        .is_some_and(|conditions| {
                            conditions.iter().any(|condition| {
                                condition.type_ == "Established" && condition.status == "True"
                            })
                        }))
                }
            })
            .await?;

        let valid: DynamicObject = serde_json::from_value(json!({
            "apiVersion": format!("{group}/v1"),
            "kind": "Widget",
            "metadata": {"name": widget_name.clone(), "namespace": &context.namespace},
            "spec": {"color": "blue", "replicas": 2}
        }))?;
        widgets
            .create(&PostParams::default(), &valid)
            .await
            .context("creating a CRD object that satisfies its schema")?;

        for (description, object) in [
            (
                "an enum-invalid CRD object",
                json!({
                    "apiVersion": format!("{group}/v1"),
                    "kind": "Widget",
                    "metadata": {"name": format!("{widget_name}-enum"), "namespace": &context.namespace},
                    "spec": {"color": "red", "replicas": 2}
                }),
            ),
            (
                "a maximum-invalid CRD object",
                json!({
                    "apiVersion": format!("{group}/v1"),
                    "kind": "Widget",
                    "metadata": {"name": format!("{widget_name}-maximum"), "namespace": &context.namespace},
                    "spec": {"color": "blue", "replicas": 4}
                }),
            ),
        ] {
            let object: DynamicObject = serde_json::from_value(object)?;
            match widgets.create(&PostParams::default(), &object).await {
                Err(KubeError::Api(error)) if error.code == 422 => {}
                Err(error) => anyhow::bail!("{description} returned the wrong API error: {error}"),
                Ok(_) => anyhow::bail!("{description} was accepted"),
            }
        }
        Ok::<(), anyhow::Error>(())
    }
    .await;

    let _ = widgets.delete(&widget_name, &DeleteParams::default()).await;
    let _ = crds.delete(&crd_name, &DeleteParams::default()).await;
    result
}

pub(super) async fn nodeapiserver_mutating_admission_policy_mutates_create(
    context: &E2eContext,
) -> Result<()> {
    let cfg = crate::config::Config::from_env()?;
    if !matches!(cfg.target, crate::config::Target::NodeApiserver) {
        return Err(skip_test("MutatingAdmissionPolicy enforcement is a nodeapiserver-only check"));
    }

    let suffix = std::process::id();
    let policy_name = format!("nodeapiserver-map-{suffix}");
    let binding_name = format!("nodeapiserver-map-binding-{suffix}");
    let object_name = format!("nodeapiserver-map-object-{suffix}");
    let configmaps: Api<ConfigMap> = Api::namespaced(context.client.clone(), &context.namespace);

    let policy = json!({
        "apiVersion": "admissionregistration.k8s.io/v1alpha1",
        "kind": "MutatingAdmissionPolicy",
        "metadata": {"name": policy_name},
        "spec": {
            "matchConstraints": {
                "resourceRules": [{
                    "apiGroups": [""],
                    "apiVersions": ["v1"],
                    "operations": ["CREATE"],
                    "resources": ["configmaps"]
                }]
            },
            "mutations": [{
                "patchType": "JSONPatch",
                "jsonPatch": {
                    "expression": "[JSONPatch{op: \"add\", path: \"/metadata/finalizers/-\", value: \"nodeapiserver.test\"}]"
                }
            }, {
                "patchType": "ApplyConfiguration",
                "applyConfiguration": {
                    "expression": "Object{metadata: Object.metadata{labels: {\"typed-mutation\": \"true\"}}}"
                }
            }]
        }
    });
    let create_policy = Request::builder()
        .method("POST")
        .uri("/apis/admissionregistration.k8s.io/v1alpha1/mutatingadmissionpolicies")
        .header("Content-Type", "application/json")
        .body(serde_json::to_vec(&policy)?)?;
    context
        .client
        .request::<Value>(create_policy)
        .await
        .context("creating the e2e MutatingAdmissionPolicy")?;

    let binding = json!({
        "apiVersion": "admissionregistration.k8s.io/v1alpha1",
        "kind": "MutatingAdmissionPolicyBinding",
        "metadata": {"name": binding_name},
        "spec": {"policyName": policy_name}
    });
    let create_binding = Request::builder()
        .method("POST")
        .uri("/apis/admissionregistration.k8s.io/v1alpha1/mutatingadmissionpolicybindings")
        .header("Content-Type", "application/json")
        .body(serde_json::to_vec(&binding)?)?;
    context
        .client
        .request::<Value>(create_binding)
        .await
        .context("creating the e2e MutatingAdmissionPolicyBinding")?;

    let result = async {
        let created = configmaps
            .create(
                &PostParams::default(),
                &ConfigMap {
                    metadata: kube::api::ObjectMeta {
                        name: Some(object_name.clone()),
                        labels: Some(BTreeMap::new()),
                        finalizers: Some(Vec::new()),
                        ..Default::default()
                    },
                    ..Default::default()
                },
            )
            .await
            .context("creating the MutatingAdmissionPolicy probe ConfigMap")?;
        let finalizers = created.metadata.finalizers.as_deref().unwrap_or(&[]);
        anyhow::ensure!(
            finalizers.len() == 1 && finalizers[0] == "nodeapiserver.test",
            "MutatingAdmissionPolicy was not applied exactly once: {:?}",
            finalizers
        );
        anyhow::ensure!(
            created
                .metadata
                .labels
                .as_ref()
                .and_then(|labels| labels.get("typed-mutation"))
                .is_some_and(|value| value == "true"),
            "typed ApplyConfiguration mutation was not applied: {:?}",
            created.metadata.labels
        );
        Ok::<(), anyhow::Error>(())
    }
    .await;

    let _ = configmaps.delete(&object_name, &DeleteParams::default()).await;
    for (resource, name) in [
        ("mutatingadmissionpolicybindings", binding_name),
        ("mutatingadmissionpolicies", policy_name),
    ] {
        let request = Request::builder()
            .method("DELETE")
            .uri(format!("/apis/admissionregistration.k8s.io/v1alpha1/{resource}/{name}"))
            .body(Vec::new())?;
        let _ = context.client.request::<Value>(request).await;
    }
    result
}

pub(super) async fn nodeapiserver_validates_crd_status_subresource(
    context: &E2eContext,
) -> Result<()> {
    let cfg = crate::config::Config::from_env()?;
    if !matches!(cfg.target, crate::config::Target::NodeApiserver) {
        return Err(skip_test(
            "CRD status-subresource validation is a nodeapiserver-only check",
        ));
    }

    let suffix = std::process::id();
    let group = format!("nodeapiserver-status-{suffix}.test");
    let crd_name = format!("widgets.{group}");
    let widget_name = format!("widget-{suffix}");
    let crds: Api<CustomResourceDefinition> = Api::all(context.client.clone());
    let widgets: Api<DynamicObject> = Api::namespaced_with(
        context.client.clone(),
        &context.namespace,
        &ApiResource::from_gvk(&GroupVersionKind::gvk(&group, "v1", "Widget")),
    );
    let crd: CustomResourceDefinition = serde_json::from_value(json!({
        "apiVersion": "apiextensions.k8s.io/v1",
        "kind": "CustomResourceDefinition",
        "metadata": {"name": crd_name},
        "spec": {
            "group": group.clone(),
            "names": {
                "plural": "widgets",
                "singular": "widget",
                "kind": "Widget",
                "listKind": "WidgetList"
            },
            "scope": "Namespaced",
            "versions": [{
                "name": "v1",
                "served": true,
                "storage": true,
                "schema": {"openAPIV3Schema": {
                    "type": "object",
                    "required": ["spec"],
                    "properties": {
                        "spec": {
                            "type": "object",
                            "required": ["color"],
                            "properties": {"color": {"type": "string"}}
                        },
                        "status": {
                            "type": "object",
                            "required": ["phase"],
                            "properties": {
                                "phase": {"type": "string", "enum": ["Ready", "Failed"]}
                            }
                        }
                    }
                }},
                "subresources": {"status": {}}
            }]
        }
    }))?;

    let result = async {
        crds.create(&PostParams::default(), &crd)
            .await
            .context("creating a CRD with a status schema")?;
        context
            .wait_until(
                "CRD status schema resource to become established",
                Duration::from_secs(60),
                || {
                    let crds = crds.clone();
                    let crd_name = crd_name.clone();
                    async move {
                        Ok(crds
                            .get_opt(&crd_name)
                            .await?
                            .and_then(|crd| crd.status)
                            .and_then(|status| status.conditions)
                            .is_some_and(|conditions| {
                                conditions.iter().any(|condition| {
                                    condition.type_ == "Established"
                                        && condition.status == "True"
                                })
                            }))
                    }
                },
            )
            .await?;

        let widget: DynamicObject = serde_json::from_value(json!({
            "apiVersion": format!("{group}/v1"),
            "kind": "Widget",
            "metadata": {"name": widget_name.clone(), "namespace": &context.namespace},
            "spec": {"color": "blue"}
        }))?;
        let created = widgets
            .create(&PostParams::default(), &widget)
            .await
            .context("creating the CRD status-subresource fixture")?;
        let resource_version = created
            .metadata
            .resource_version
            .context("CRD status-subresource fixture had no resourceVersion")?;

        let status_uri = format!(
            "/apis/{group}/v1/namespaces/{}/widgets/{}/status",
            context.namespace, widget_name
        );
        let invalid = json!({
            "apiVersion": format!("{group}/v1"),
            "kind": "Widget",
            "metadata": {"name": widget_name, "namespace": context.namespace, "resourceVersion": resource_version},
            "status": {"phase": "Unknown"}
        });
        let request = Request::builder()
            .method("PUT")
            .uri(&status_uri)
            .header("content-type", "application/json")
            .body(serde_json::to_vec(&invalid)?)?;
        match context.client.request::<Value>(request).await {
            Err(KubeError::Api(error)) if error.code == 422 => {}
            Err(error) => {
                anyhow::bail!("invalid CRD status returned the wrong API error: {error}")
            }
            Ok(value) => anyhow::bail!("invalid CRD status was accepted: {value}"),
        }

        let valid = json!({
            "apiVersion": format!("{group}/v1"),
            "kind": "Widget",
            "metadata": {"name": widget_name, "namespace": context.namespace, "resourceVersion": resource_version},
            "status": {"phase": "Ready", "unknown": "prune me"}
        });
        let request = Request::builder()
            .method("PUT")
            .uri(&status_uri)
            .header("content-type", "application/json")
            .body(serde_json::to_vec(&valid)?)?;
        let updated: Value = context
            .client
            .request(request)
            .await
            .context("updating a valid CRD status subresource")?;
        anyhow::ensure!(
            updated.pointer("/status/phase").and_then(Value::as_str) == Some("Ready"),
            "valid CRD status update returned the wrong phase: {updated}"
        );
        anyhow::ensure!(
            updated.pointer("/status/unknown").is_none(),
            "unknown CRD status field was not pruned: {updated}"
        );
        Ok::<(), anyhow::Error>(())
    }
    .await;

    let _ = widgets.delete(&widget_name, &DeleteParams::default()).await;
    let _ = crds.delete(&crd_name, &DeleteParams::default()).await;
    result
}

fn serve_webhook_connection(
    mut stream: std::net::TcpStream,
    calls: &AtomicUsize,
) -> std::io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    let mut request = Vec::new();
    let mut buffer = [0_u8; 4096];
    let content_length = loop {
        let read = stream.read(&mut buffer)?;
        if read == 0 {
            return Ok(());
        }
        request.extend_from_slice(&buffer[..read]);
        let Some(headers_end) = request
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
        else {
            continue;
        };
        let headers = String::from_utf8_lossy(&request[..headers_end]);
        break headers
            .lines()
            .find_map(|line| {
                line.strip_prefix("Content-Length:")
                    .or_else(|| line.strip_prefix("content-length:"))
            })
            .and_then(|value| value.trim().parse::<usize>().ok())
            .unwrap_or(0);
    };
    let headers_end = request
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .expect("HTTP headers were found above")
        + 4;
    while request.len() < headers_end + content_length {
        let read = stream.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        request.extend_from_slice(&buffer[..read]);
    }
    let review: Value = serde_json::from_slice(&request[headers_end..headers_end + content_length])
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
    let uid = review
        .pointer("/request/uid")
        .and_then(Value::as_str)
        .unwrap_or("");
    let body = serde_json::to_vec(&json!({
        "apiVersion": "admission.k8s.io/v1",
        "kind": "AdmissionReview",
        "response": {"uid": uid, "allowed": true}
    }))
    .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
    write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )?;
    stream.write_all(&body)?;
    calls.fetch_add(1, Ordering::SeqCst);
    Ok(())
}

fn serve_authorization_webhook_connection(
    mut stream: std::net::TcpStream,
    reviews: &Mutex<Vec<Value>>,
    denied_name: &str,
) -> std::io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    let mut request = Vec::new();
    let mut buffer = [0_u8; 4096];
    let content_length = loop {
        let read = stream.read(&mut buffer)?;
        if read == 0 {
            return Ok(());
        }
        request.extend_from_slice(&buffer[..read]);
        let Some(headers_end) = request
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
        else {
            continue;
        };
        let headers = String::from_utf8_lossy(&request[..headers_end]);
        break headers
            .lines()
            .find_map(|line| {
                line.strip_prefix("Content-Length:")
                    .or_else(|| line.strip_prefix("content-length:"))
            })
            .and_then(|value| value.trim().parse::<usize>().ok())
            .unwrap_or(0);
    };
    let headers_end = request
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .expect("HTTP headers were found above")
        + 4;
    while request.len() < headers_end + content_length {
        let read = stream.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        request.extend_from_slice(&buffer[..read]);
    }
    if request.len() < headers_end + content_length {
        return Err(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "authorization webhook request body was truncated",
        ));
    }
    let review: Value = serde_json::from_slice(&request[headers_end..headers_end + content_length])
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
    let decision = if review.pointer("/spec/nonResourceAttributes/path").and_then(Value::as_str) == Some("/healthz") {
        json!({"allowed": false, "denied": false})
    } else if review.pointer("/spec/resourceAttributes/name").and_then(Value::as_str) == Some(denied_name) {
        json!({"allowed": false, "denied": true})
    } else {
        json!({"allowed": true, "denied": false})
    };
    if let Ok(mut seen) = reviews.lock() {
        seen.push(review);
    }
    let body = serde_json::to_vec(&json!({
        "apiVersion": "authorization.k8s.io/v1",
        "kind": "SubjectAccessReview",
        "status": decision,
    }))
    .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
    write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )?;
    stream.write_all(&body)
}

pub(super) async fn nodeapiserver_honors_webhook_match_conditions(
    context: &E2eContext,
) -> Result<()> {
    let cfg = crate::config::Config::from_env()?;
    if !matches!(cfg.target, crate::config::Target::NodeApiserver) {
        return Err(skip_test(
            "admission webhook checks are only exercised against nodeapiserver",
        ));
    }

    let listener = TcpListener::bind(("127.0.0.1", 0))
        .context("binding the e2e admission webhook")?;
    listener.set_nonblocking(true)?;
    let address = listener.local_addr()?;
    let calls = Arc::new(AtomicUsize::new(0));
    let stopping = Arc::new(AtomicBool::new(false));
    let server_calls = calls.clone();
    let server_stopping = stopping.clone();
    let server = thread::spawn(move || {
        while !server_stopping.load(Ordering::Relaxed) {
            match listener.accept() {
                Ok((stream, _)) => {
                    let _ = serve_webhook_connection(stream, &server_calls);
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(5));
                }
                Err(_) => break,
            }
        }
    });

    let suffix = std::process::id();
    let configuration_name = format!("nodeapiserver-webhook-{suffix}");
    let skip_name = format!("nodeapiserver-webhook-skip-{suffix}");
    let match_name = format!("nodeapiserver-webhook-match-{suffix}");
    let configuration = json!({
        "apiVersion": "admissionregistration.k8s.io/v1",
        "kind": "ValidatingWebhookConfiguration",
        "metadata": {"name": configuration_name},
        "webhooks": [{
            "name": "matchconditions.nodeapiserver.test",
            "admissionReviewVersions": ["v1"],
            "sideEffects": "None",
            "failurePolicy": "Fail",
            "timeoutSeconds": 5,
            "clientConfig": {"url": format!("http://{}", address)},
            "rules": [{
                "operations": ["CREATE"],
                "apiGroups": [""],
                "apiVersions": ["v1"],
                "resources": ["configmaps"],
                "scope": "Namespaced"
            }],
            "matchConditions": [{
                "name": "only-the-canary",
                "expression": format!("object.metadata.name == '{match_name}'")
            }]
        }]
    });
    let configmaps: Api<ConfigMap> = Api::namespaced(context.client.clone(), &context.namespace);
    let create_configuration = Request::builder()
        .method("POST")
        .uri("/apis/admissionregistration.k8s.io/v1/validatingwebhookconfigurations")
        .header("Content-Type", "application/json")
        .body(serde_json::to_vec(&configuration)?)?;
    let result = async {
        context
            .client
            .request::<Value>(create_configuration)
            .await
            .context("creating the matchConditions webhook configuration")?;
        configmaps
            .create(
                &PostParams::default(),
                &ConfigMap {
                    metadata: kube::core::ObjectMeta {
                        name: Some(skip_name.clone()),
                        ..Default::default()
                    },
                    ..Default::default()
                },
            )
            .await
            .context("creating the nonmatching webhook ConfigMap")?;
        anyhow::ensure!(
            calls.load(Ordering::SeqCst) == 0,
            "matchConditions webhook was invoked for a nonmatching object"
        );
        configmaps
            .create(
                &PostParams::default(),
                &ConfigMap {
                    metadata: kube::core::ObjectMeta {
                        name: Some(match_name.clone()),
                        ..Default::default()
                    },
                    ..Default::default()
                },
            )
            .await
            .context("creating the matching webhook ConfigMap")?;
        context
            .wait_until("matchConditions webhook invocation", Duration::from_secs(30), || {
                let calls = calls.clone();
                async move { Ok(calls.load(Ordering::SeqCst) == 1) }
            })
            .await?;
        Ok::<(), anyhow::Error>(())
    }
    .await;

    let _ = configmaps.delete(&skip_name, &DeleteParams::default()).await;
    let _ = configmaps.delete(&match_name, &DeleteParams::default()).await;
    let delete_configuration = Request::builder()
        .method("DELETE")
        .uri(format!(
            "/apis/admissionregistration.k8s.io/v1/validatingwebhookconfigurations/{configuration_name}"
        ))
        .body(Vec::new())?;
    let _ = context.client.request::<Value>(delete_configuration).await;
    stopping.store(true, Ordering::Relaxed);
    let _ = server.join();
    result
}

pub(super) async fn nodeapiserver_honors_webhook_side_effects_on_dry_run(
    context: &E2eContext,
) -> Result<()> {
    let cfg = crate::config::Config::from_env()?;
    if !matches!(cfg.target, crate::config::Target::NodeApiserver) {
        return Err(skip_test(
            "admission webhook checks are only exercised against nodeapiserver",
        ));
    }

    let listener = TcpListener::bind(("127.0.0.1", 0))
        .context("binding the e2e admission webhook")?;
    listener.set_nonblocking(true)?;
    let address = listener.local_addr()?;
    let calls = Arc::new(AtomicUsize::new(0));
    let stopping = Arc::new(AtomicBool::new(false));
    let server_calls = calls.clone();
    let server_stopping = stopping.clone();
    let server = thread::spawn(move || {
        while !server_stopping.load(Ordering::Relaxed) {
            match listener.accept() {
                Ok((stream, _)) => {
                    let _ = serve_webhook_connection(stream, &server_calls);
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(5));
                }
                Err(_) => break,
            }
        }
    });

    let suffix = std::process::id();
    let rejecting_configuration_name = format!("nodeapiserver-webhook-side-effects-{suffix}");
    let allowing_configuration_name = format!("nodeapiserver-webhook-none-on-dry-run-{suffix}");
    let rejecting_name = format!("nodeapiserver-dry-run-rejected-{suffix}");
    let allowing_name = format!("nodeapiserver-dry-run-allowed-{suffix}");
    let configuration = |name: &str, webhook_name: &str, side_effects: &str| {
        json!({
            "apiVersion": "admissionregistration.k8s.io/v1",
            "kind": "ValidatingWebhookConfiguration",
            "metadata": {"name": name},
            "webhooks": [{
                "name": webhook_name,
                "admissionReviewVersions": ["v1"],
                "sideEffects": side_effects,
                "failurePolicy": "Fail",
                "timeoutSeconds": 5,
                "clientConfig": {"url": format!("http://{}", address)},
                "rules": [{
                    "operations": ["CREATE"],
                    "apiGroups": [""],
                    "apiVersions": ["v1"],
                    "resources": ["configmaps"],
                    "scope": "Namespaced"
                }]
            }]
        })
    };
    let configmaps: Api<ConfigMap> = Api::namespaced(context.client.clone(), &context.namespace);
    let rejecting_configuration = configuration(
        &rejecting_configuration_name,
        "side-effects.nodeapiserver.test",
        "Some",
    );
    let allowing_configuration = configuration(
        &allowing_configuration_name,
        "none-on-dry-run.nodeapiserver.test",
        "NoneOnDryRun",
    );
    let create_configuration = |configuration: Value| async move {
        context
            .client
            .request::<Value>(
                Request::builder()
                    .method("POST")
                    .uri("/apis/admissionregistration.k8s.io/v1/validatingwebhookconfigurations")
                    .header("Content-Type", "application/json")
                    .body(serde_json::to_vec(&configuration)?)?,
            )
            .await
            .map(|_| ())
            .map_err(anyhow::Error::from)
    };
    let delete_configuration_request = |name: &str| {
        Request::builder()
            .method("DELETE")
            .uri(format!(
                "/apis/admissionregistration.k8s.io/v1/validatingwebhookconfigurations/{name}"
            ))
            .body(Vec::new())
    };
    let result = async {
        create_configuration(rejecting_configuration).await?;
        let mut dry_run_params = PostParams::default();
        dry_run_params.dry_run = true;
        match configmaps
            .create(
                &dry_run_params,
                &ConfigMap {
                    metadata: kube::core::ObjectMeta {
                        name: Some(rejecting_name.clone()),
                        ..Default::default()
                    },
                    ..Default::default()
                },
            )
            .await
        {
            Err(KubeError::Api(error)) => anyhow::ensure!(
                error.code == 400,
                "dry-run with sideEffects=Some returned the wrong status: {error}"
            ),
            Err(error) => anyhow::bail!(
                "dry-run with sideEffects=Some returned a non-API error: {error}"
            ),
            Ok(object) => anyhow::bail!(
                "dry-run with sideEffects=Some unexpectedly succeeded: {object:?}"
            ),
        }
        anyhow::ensure!(
            calls.load(Ordering::SeqCst) == 0,
            "a side-effecting webhook was invoked for a dry-run request"
        );

        context
            .client
            .request::<Value>(delete_configuration_request(&rejecting_configuration_name)?)
            .await
            .context("deleting the sideEffects=Some webhook configuration")?;
        create_configuration(allowing_configuration).await?;
        let created = configmaps
            .create(
                &dry_run_params,
                &ConfigMap {
                    metadata: kube::core::ObjectMeta {
                        name: Some(allowing_name.clone()),
                        ..Default::default()
                    },
                    ..Default::default()
                },
            )
            .await
            .context("dry-running a ConfigMap against sideEffects=NoneOnDryRun")?;
        anyhow::ensure!(
            created.metadata.name.as_deref() == Some(allowing_name.as_str()),
            "NoneOnDryRun webhook returned the wrong object: {created:?}"
        );
        context
            .wait_until("NoneOnDryRun webhook invocation", Duration::from_secs(30), || {
                let calls = calls.clone();
                async move { Ok(calls.load(Ordering::SeqCst) == 1) }
            })
            .await?;
        match configmaps.get(&allowing_name).await {
            Err(KubeError::Api(error)) if error.code == 404 => {}
            Err(error) => anyhow::bail!(
                "NoneOnDryRun ConfigMap lookup returned an unexpected error: {error}"
            ),
            Ok(object) => anyhow::bail!(
                "NoneOnDryRun dry-run ConfigMap was persisted: {object:?}"
            ),
        }
        Ok::<(), anyhow::Error>(())
    }
    .await;

    let _ = configmaps.delete(&rejecting_name, &DeleteParams::default()).await;
    let _ = configmaps.delete(&allowing_name, &DeleteParams::default()).await;
    for name in [&rejecting_configuration_name, &allowing_configuration_name] {
        if let Ok(request) = delete_configuration_request(name) {
            let _ = context.client.request::<Value>(request).await;
        }
    }
    stopping.store(true, Ordering::Relaxed);
    let _ = server.join();
    result
}

pub(super) async fn nodeapiserver_runs_webhook_for_delete_collection(
    context: &E2eContext,
) -> Result<()> {
    let cfg = crate::config::Config::from_env()?;
    if !matches!(cfg.target, crate::config::Target::NodeApiserver) {
        return Err(skip_test(
            "admission webhook checks are only exercised against nodeapiserver",
        ));
    }

    let listener = TcpListener::bind(("127.0.0.1", 0))
        .context("binding the e2e admission webhook")?;
    listener.set_nonblocking(true)?;
    let address = listener.local_addr()?;
    let calls = Arc::new(AtomicUsize::new(0));
    let stopping = Arc::new(AtomicBool::new(false));
    let server_calls = calls.clone();
    let server_stopping = stopping.clone();
    let server = thread::spawn(move || {
        while !server_stopping.load(Ordering::Relaxed) {
            match listener.accept() {
                Ok((stream, _)) => {
                    let _ = serve_webhook_connection(stream, &server_calls);
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(5));
                }
                Err(_) => break,
            }
        }
    });

    let suffix = std::process::id();
    let configuration_name = format!("nodeapiserver-webhook-deletecollection-{suffix}");
    let label_key = "nodeapiserver-delete-collection";
    let label_value = "true";
    let first_name = format!("nodeapiserver-delete-collection-first-{suffix}");
    let second_name = format!("nodeapiserver-delete-collection-second-{suffix}");
    let configuration = json!({
        "apiVersion": "admissionregistration.k8s.io/v1",
        "kind": "ValidatingWebhookConfiguration",
        "metadata": {"name": configuration_name},
        "webhooks": [{
            "name": "delete-collection.nodeapiserver.test",
            "admissionReviewVersions": ["v1"],
            "sideEffects": "None",
            "failurePolicy": "Fail",
            "timeoutSeconds": 5,
            "clientConfig": {"url": format!("http://{}", address)},
            "rules": [{
                "operations": ["DELETE"],
                "apiGroups": [""],
                "apiVersions": ["v1"],
                "resources": ["configmaps"],
                "scope": "Namespaced"
            }],
            "matchConditions": [{
                "name": "delete-object-is-null",
                "expression": "object == null"
            }]
        }]
    });
    let configmaps: Api<ConfigMap> = Api::namespaced(context.client.clone(), &context.namespace);
    let result = async {
        context
            .client
            .request::<Value>(
                Request::builder()
                    .method("POST")
                    .uri("/apis/admissionregistration.k8s.io/v1/validatingwebhookconfigurations")
                    .header("Content-Type", "application/json")
                    .body(serde_json::to_vec(&configuration)?)?,
            )
            .await
            .context("creating the deletecollection webhook configuration")?;
        for name in [&first_name, &second_name] {
            configmaps
                .create(
                    &PostParams::default(),
                    &ConfigMap {
                        metadata: kube::core::ObjectMeta {
                            name: Some(name.clone()),
                            labels: Some(BTreeMap::from([(
                                label_key.to_string(),
                                label_value.to_string(),
                            )])),
                            ..Default::default()
                        },
                        ..Default::default()
                    },
                )
                .await
                .with_context(|| format!("creating deletecollection ConfigMap {name}"))?;
        }
        configmaps
            .delete_collection(
                &DeleteParams::default(),
                &ListParams::default().labels(&format!("{label_key}={label_value}")),
            )
            .await
            .context("deleting ConfigMaps through deletecollection")?;
        context
            .wait_until("deletecollection webhook invocations", Duration::from_secs(30), || {
                let calls = calls.clone();
                async move { Ok(calls.load(Ordering::SeqCst) == 2) }
            })
            .await?;
        anyhow::ensure!(
            configmaps
                .list(&ListParams::default().labels(&format!("{label_key}={label_value}")))
                .await?
                .items
                .is_empty(),
            "deletecollection left matching ConfigMaps behind"
        );
        Ok::<(), anyhow::Error>(())
    }
    .await;

    let _ = configmaps.delete(&first_name, &DeleteParams::default()).await;
    let _ = configmaps.delete(&second_name, &DeleteParams::default()).await;
    if let Ok(request) = Request::builder()
        .method("DELETE")
        .uri(format!(
            "/apis/admissionregistration.k8s.io/v1/validatingwebhookconfigurations/{configuration_name}"
        ))
        .body(Vec::new())
    {
        let _ = context.client.request::<Value>(request).await;
    }
    stopping.store(true, Ordering::Relaxed);
    let _ = server.join();
    result
}

pub(super) async fn nodeapiserver_honors_finalizers(context: &E2eContext) -> Result<()> {
    let cfg = crate::config::Config::from_env()?;
    if !matches!(cfg.target, crate::config::Target::NodeApiserver) {
        return Err(skip_test("finalizer semantics are a nodeapiserver-only check"));
    }

    let name = format!("nodeapiserver-finalizer-{}", std::process::id());
    let configmaps: Api<ConfigMap> = Api::namespaced(context.client.clone(), &context.namespace);
    let configmap = ConfigMap {
        metadata: kube::api::ObjectMeta {
            name: Some(name.clone()),
            finalizers: Some(vec!["nodeapiserver.test/finalizer".to_string()]),
            ..Default::default()
        },
        ..Default::default()
    };

    let result = async {
        configmaps
            .create(&PostParams::default(), &configmap)
            .await
            .context("creating the finalizer ConfigMap")?;
        configmaps
            .delete(&name, &DeleteParams::default())
            .await
            .context("deleting the finalizer ConfigMap")?;
        context
            .wait_until("nodeapiserver finalizer deletion timestamp", Duration::from_secs(30), || {
                let configmaps = configmaps.clone();
                let name = name.clone();
                async move {
                    let object = configmaps.get(&name).await?;
                    Ok(object.metadata.deletion_timestamp.is_some()
                        && object
                            .metadata
                            .finalizers
                            .as_deref()
                            .is_some_and(|finalizers| {
                                finalizers.len() == 1
                                    && finalizers[0] == "nodeapiserver.test/finalizer"
                            }))
                }
            })
            .await?;
        configmaps
            .patch(
                &name,
                &PatchParams::default(),
                &Patch::Merge(&json!({"metadata": {"finalizers": null}})),
            )
            .await
            .context("removing the finalizer")?;
        context
            .wait_until("nodeapiserver finalizer deletion", Duration::from_secs(30), || {
                let configmaps = configmaps.clone();
                let name = name.clone();
                async move { Ok(configmaps.get_opt(&name).await?.is_none()) }
            })
            .await
    }
    .await;

    let _ = configmaps
        .patch(
            &name,
            &PatchParams::default(),
            &Patch::Merge(&json!({"metadata": {"finalizers": null}})),
        )
        .await;
    let _ = configmaps.delete(&name, &DeleteParams::default()).await;
    result
}

pub(super) async fn nodeapiserver_honors_authorization_webhook_decisions(
    context: &E2eContext,
) -> Result<()> {
    let cfg = crate::config::Config::from_env()?;
    if !matches!(cfg.target, crate::config::Target::NodeApiserver) {
        return Err(skip_test(
            "authorization webhook checks are only exercised against nodeapiserver",
        ));
    }

    let listener = TcpListener::bind(("127.0.0.1", 0))
        .context("binding the e2e authorization webhook")?;
    listener.set_nonblocking(true)?;
    let address = listener.local_addr()?;
    let reviews = Arc::new(Mutex::new(Vec::new()));
    let stopping = Arc::new(AtomicBool::new(false));
    let server_reviews = reviews.clone();
    let server_stopping = stopping.clone();
    let denied_name = format!("nodeapiserver-authz-denied-{}", std::process::id());
    let allowed_name = format!("nodeapiserver-authz-allowed-{}", std::process::id());
    let server_denied_name = denied_name.clone();
    let server = thread::spawn(move || {
        while !server_stopping.load(Ordering::Relaxed) {
            match listener.accept() {
                Ok((stream, _)) => {
                    let _ = serve_authorization_webhook_connection(
                        stream,
                        &server_reviews,
                        &server_denied_name,
                    );
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(5));
                }
                Err(_) => break,
            }
        }
    });

    let webhook = match NodeapiserverAuthorizationWebhookOverride::install(&format!("http://{address}")) {
        Ok(webhook) => webhook,
        Err(error) => {
            stopping.store(true, Ordering::Relaxed);
            let _ = server.join();
            return Err(error);
        }
    };
    let result = async {
        context
            .wait_until(
                "nodeapiserver to allow a NoOpinion authorization webhook response",
                Duration::from_secs(60),
                || async {
                    let output = Command::new("curl")
                        .args([
                            "-k",
                            "-sS",
                            "--max-time",
                            "10",
                            "-o",
                            "/dev/null",
                            "-w",
                            "%{http_code}",
                            "https://127.0.0.1:6443/healthz",
                        ])
                        .output();
                    Ok(output.is_ok_and(|output| {
                        output.status.success() && String::from_utf8_lossy(&output.stdout).trim() == "200"
                    }))
                },
            )
            .await?;

        let denied_url = format!(
            "https://127.0.0.1:6443/api/v1/namespaces/{}/configmaps/{denied_name}",
            context.namespace
        );
        let denied = Command::new("curl")
            .args([
                "-k",
                "-sS",
                "--max-time",
                "10",
                "-o",
                "/dev/null",
                "-w",
                "%{http_code}",
                &denied_url,
            ])
            .output()
            .context("checking an authorization webhook denial")?;
        anyhow::ensure!(
            denied.status.success() && String::from_utf8_lossy(&denied.stdout).trim() == "403",
            "authorization webhook Deny did not produce HTTP 403: stdout={} stderr={}",
            String::from_utf8_lossy(&denied.stdout),
            String::from_utf8_lossy(&denied.stderr)
        );

        let allowed_url = format!(
            "https://127.0.0.1:6443/api/v1/namespaces/{}/configmaps/{allowed_name}",
            context.namespace
        );
        let allowed = Command::new("curl")
            .args([
                "-k",
                "-sS",
                "--max-time",
                "10",
                "-o",
                "/dev/null",
                "-w",
                "%{http_code}",
                &allowed_url,
            ])
            .output()
            .context("checking an authorization webhook allow")?;
        anyhow::ensure!(
            allowed.status.success() && String::from_utf8_lossy(&allowed.stdout).trim() == "404",
            "authorization webhook Allow did not bypass local RBAC: stdout={} stderr={}",
            String::from_utf8_lossy(&allowed.stdout),
            String::from_utf8_lossy(&allowed.stderr)
        );

        context
            .wait_until(
                "authorization webhook to receive all test requests",
                Duration::from_secs(30),
                || {
                    let reviews = reviews.clone();
                    async move { Ok(reviews.lock().is_ok_and(|reviews| reviews.len() >= 3)) }
                },
            )
            .await?;
        let reviews = reviews
            .lock()
            .map_err(|_| anyhow::anyhow!("authorization webhook review list was poisoned"))?;
        anyhow::ensure!(
            reviews.iter().any(|review| {
                review.pointer("/spec/nonResourceAttributes/path").and_then(Value::as_str) == Some("/healthz")
                    && review.pointer("/spec/nonResourceAttributes/verb").and_then(Value::as_str) == Some("get")
            }),
            "authorization webhook did not receive the expected non-resource attributes: {reviews:?}"
        );
        anyhow::ensure!(
            reviews.iter().any(|review| {
                review.pointer("/spec/resourceAttributes/resource").and_then(Value::as_str) == Some("configmaps")
                    && review.pointer("/spec/resourceAttributes/name").and_then(Value::as_str) == Some(denied_name.as_str())
            }),
            "authorization webhook did not receive the expected resource attributes: {reviews:?}"
        );
        anyhow::ensure!(
            reviews.iter().any(|review| {
                review.pointer("/spec/resourceAttributes/resource").and_then(Value::as_str) == Some("configmaps")
                    && review.pointer("/spec/resourceAttributes/name").and_then(Value::as_str) == Some(allowed_name.as_str())
            }),
            "authorization webhook did not receive the allowed resource request: {reviews:?}"
        );
        Ok::<(), anyhow::Error>(())
    }
    .await;

    drop(webhook);
    stopping.store(true, Ordering::Relaxed);
    let _ = server.join();
    result
}

pub(super) async fn nodeapiserver_honors_resource_version_snapshot(
    context: &E2eContext,
) -> Result<()> {
    let cfg = crate::config::Config::from_env()?;
    if !matches!(cfg.target, crate::config::Target::NodeApiserver) {
        return Err(skip_test(
            "resourceVersion snapshot checks are only exercised against nodeapiserver",
        ));
    }

    let name = format!("nodeapiserver-rv-{}", std::process::id());
    let configmaps: Api<ConfigMap> = Api::namespaced(context.client.clone(), &context.namespace);
    let mut initial_data = BTreeMap::new();
    initial_data.insert("value".to_string(), "before".to_string());
    let created = configmaps
        .create(
            &PostParams::default(),
            &ConfigMap {
                metadata: k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta {
                    name: Some(name.clone()),
                    ..Default::default()
                },
                data: Some(initial_data),
                ..Default::default()
            },
        )
        .await
        .context("creating the resourceVersion snapshot fixture")?;
    let resource_version = created
        .metadata
        .resource_version
        .context("resourceVersion snapshot fixture had no resourceVersion")?;

    let mut patch_data = BTreeMap::new();
    patch_data.insert("value".to_string(), "after".to_string());
    configmaps
        .patch(
            &name,
            &PatchParams::default(),
            &Patch::Merge(serde_json::json!({"data": patch_data})),
        )
        .await
        .context("updating the resourceVersion snapshot fixture")?;

    let request = Request::builder()
        .method("GET")
        .uri(format!(
            "/api/v1/namespaces/{}/configmaps/{}?resourceVersion={resource_version}",
            context.namespace, name
        ))
        .body(Vec::new())?;
    let snapshot: ConfigMap = context
        .client
        .request(request)
        .await
        .context("reading the resourceVersion snapshot")?;
    anyhow::ensure!(
        snapshot.data.as_ref().and_then(|data| data.get("value")) == Some(&"before".to_string()),
        "resourceVersion-pinned GET returned the current object instead of the requested snapshot"
    );
    configmaps
        .delete(&name, &DeleteParams::default())
        .await
        .context("cleaning up the resourceVersion snapshot fixture")?;
    Ok(())
}

pub(super) async fn nodeapiserver_serves_partial_object_metadata(
    context: &E2eContext,
) -> Result<()> {
    let cfg = crate::config::Config::from_env()?;
    if !matches!(cfg.target, crate::config::Target::NodeApiserver) {
        return Err(skip_test(
            "the cluster is using the upstream apiserver target",
        ));
    }

    let accept = "application/json;as=PartialObjectMetadata;g=meta.k8s.io;v=v1, application/json";
    let object_request = Request::builder()
        .method("GET")
        .uri("/api/v1/namespaces/default/services/kubernetes")
        .header("Accept", accept)
        .body(Vec::new())?;
    let object: serde_json::Value = context
        .client
        .request(object_request)
        .await
        .context("requesting a PartialObjectMetadata Service")?;
    anyhow::ensure!(
        object.get("apiVersion").and_then(|value| value.as_str()) == Some("meta.k8s.io/v1")
            && object.get("kind").and_then(|value| value.as_str())
                == Some("PartialObjectMetadata"),
        "nodeapiserver returned the wrong PartialObjectMetadata object shape: {object}"
    );
    anyhow::ensure!(
        object.pointer("/metadata/name").and_then(|value| value.as_str()) == Some("kubernetes"),
        "PartialObjectMetadata object lost the Service metadata: {object}"
    );
    anyhow::ensure!(
        object.get("spec").is_none() && object.get("status").is_none(),
        "PartialObjectMetadata response included the full object: {object}"
    );

    let list_request = Request::builder()
        .method("GET")
        .uri("/api/v1/namespaces/default/services")
        .header("Accept", accept)
        .body(Vec::new())?;
    let list: serde_json::Value = context
        .client
        .request(list_request)
        .await
        .context("requesting a PartialObjectMetadata ServiceList")?;
    anyhow::ensure!(
        list.get("apiVersion").and_then(|value| value.as_str()) == Some("meta.k8s.io/v1")
            && list.get("kind").and_then(|value| value.as_str())
                == Some("PartialObjectMetadataList"),
        "nodeapiserver returned the wrong PartialObjectMetadataList shape: {list}"
    );
    let items = list
        .get("items")
        .and_then(|value| value.as_array())
        .context("PartialObjectMetadataList had no items array")?;
    anyhow::ensure!(
        items.iter().any(|item| {
            item.pointer("/metadata/name").and_then(|value| value.as_str()) == Some("kubernetes")
        }),
        "PartialObjectMetadataList did not include the kubernetes Service: {list}"
    );
    anyhow::ensure!(
        items
            .iter()
            .all(|item| item.get("kind").and_then(|value| value.as_str())
                == Some("PartialObjectMetadata")
                && item.get("spec").is_none()
                && item.get("status").is_none()),
        "PartialObjectMetadataList contained a full object: {list}"
    );
    Ok(())
}

pub(super) async fn nodeapiserver_watches_partial_object_metadata(
    context: &E2eContext,
) -> Result<()> {
    let cfg = crate::config::Config::from_env()?;
    if !matches!(cfg.target, crate::config::Target::NodeApiserver) {
        return Err(skip_test(
            "PartialObjectMetadata watch checks are only exercised against nodeapiserver",
        ));
    }

    let name = format!("nodeapiserver-partial-watch-{}", std::process::id());
    let configmaps: Api<ConfigMap> = Api::namespaced(context.client.clone(), &context.namespace);
    let accept = "application/json;as=PartialObjectMetadata;g=meta.k8s.io;v=v1, application/json";
    let watch_request = Request::builder()
        .method("GET")
        .uri(format!(
            "/api/v1/namespaces/{}/configmaps?watch=true&resourceVersion=0&timeoutSeconds=30&fieldSelector=metadata.name%3D{}",
            context.namespace, name
        ))
        .header("Accept", accept)
        .body(Vec::new())?;
    let mut stream = context
        .client
        .request_stream(watch_request)
        .await
        .context("starting a PartialObjectMetadata watch")?;

    let configmap: ConfigMap = serde_json::from_value(json!({
        "apiVersion": "v1",
        "kind": "ConfigMap",
        "metadata": {"name": name}
    }))?;
    let result = async {
        configmaps
            .create(&PostParams::default(), &configmap)
            .await
            .context("creating the PartialObjectMetadata watch fixture")?;

        let mut line = String::new();
        tokio::time::timeout(Duration::from_secs(30), stream.read_line(&mut line))
            .await
            .context("waiting for the PartialObjectMetadata watch event")??;
        let event: Value = serde_json::from_str(&line).context("decoding the PartialObjectMetadata watch event")?;
        anyhow::ensure!(event.get("type").and_then(|value| value.as_str()) == Some("ADDED"), "unexpected watch event: {event}");
        let object = event.get("object").context("watch event had no object")?;
        anyhow::ensure!(
            object.get("apiVersion").and_then(|value| value.as_str()) == Some("meta.k8s.io/v1")
                && object.get("kind").and_then(|value| value.as_str()) == Some("PartialObjectMetadata"),
            "watch returned the wrong PartialObjectMetadata shape: {event}"
        );
        anyhow::ensure!(
            object.pointer("/metadata/name").and_then(|value| value.as_str()) == Some(name.as_str())
                && object.get("data").is_none(),
            "watch returned more than object metadata: {event}"
        );
        Ok::<(), anyhow::Error>(())
    }
    .await;
    let _ = configmaps.delete(&name, &DeleteParams::default()).await;
    result
}

pub(super) async fn nodeapiserver_honors_generate_name(context: &E2eContext) -> Result<()> {
    let cfg = crate::config::Config::from_env()?;
    if !matches!(cfg.target, crate::config::Target::NodeApiserver) {
        return Err(skip_test("generateName checks are only exercised against nodeapiserver"));
    }

    let configmaps: Api<ConfigMap> = Api::namespaced(context.client.clone(), &context.namespace);
    let prefix = format!("nodeapiserver-generated-{}-", std::process::id());
    let created = configmaps
        .create(
            &PostParams::default(),
            &ConfigMap {
                metadata: kube::api::ObjectMeta {
                    generate_name: Some(prefix.clone()),
                    ..Default::default()
                },
                ..Default::default()
            },
        )
        .await
        .context("creating a generateName ConfigMap")?;
    let name = created
        .metadata
        .name
        .context("nodeapiserver did not return the generated ConfigMap name")?;
    anyhow::ensure!(name.starts_with(&prefix), "generated name {name:?} did not retain prefix {prefix:?}");
    anyhow::ensure!(name.len() > prefix.len(), "nodeapiserver returned no generated suffix for {name:?}");
    configmaps
        .delete(&name, &DeleteParams::default())
        .await
        .context("cleaning up the generateName ConfigMap")?;
    Ok(())
}

pub(super) async fn nodeapiserver_honors_dry_run_and_delete_preconditions(context: &E2eContext) -> Result<()> {
    let cfg = crate::config::Config::from_env()?;
    if !matches!(cfg.target, crate::config::Target::NodeApiserver) {
        return Err(skip_test("write-option checks are only exercised against nodeapiserver"));
    }

    let configmaps: Api<ConfigMap> = Api::namespaced(context.client.clone(), &context.namespace);
    let name = format!("nodeapiserver-write-options-{}", std::process::id());
    let body = json!({
        "apiVersion": "v1",
        "kind": "ConfigMap",
        "metadata": {"name": name, "namespace": context.namespace},
        "data": {"value": "dry-run"}
    });
    let request = Request::builder()
        .method("POST")
        .uri(format!("/api/v1/namespaces/{}/configmaps?dryRun=All", context.namespace))
        .header("content-type", "application/json")
        .body(serde_json::to_vec(&body)?)?;
    let dry_run: Value = context.client.request(request).await.context("creating a dry-run ConfigMap")?;
    anyhow::ensure!(dry_run.pointer("/metadata/name").and_then(Value::as_str) == Some(name.as_str()), "dry-run create returned the wrong object: {dry_run}");
    match configmaps.get(&name).await {
        Err(KubeError::Api(error)) if error.code == 404 => {}
        Err(error) => anyhow::bail!("dry-run create left an unexpected API error: {error}"),
        Ok(object) => anyhow::bail!("dry-run create persisted {:?}", object.metadata.name),
    }

    let created = configmaps
        .create(
            &PostParams::default(),
            &ConfigMap {
                metadata: kube::api::ObjectMeta {
                    name: Some(name.clone()),
                    ..Default::default()
                },
                ..Default::default()
            },
        )
        .await
        .context("creating the delete-precondition fixture")?;
    let resource_version = created
        .metadata
        .resource_version
        .context("delete-precondition fixture had no resourceVersion")?;
    let request = Request::builder()
        .method("DELETE")
        .uri(format!(
            "/api/v1/namespaces/{}/configmaps?fieldSelector=metadata.name%3D{name}&dryRun=All",
            context.namespace
        ))
        .body(Vec::new())?;
    let dry_collection: Value = context
        .client
        .request(request)
        .await
        .context("dry-running a ConfigMap collection delete")?;
    anyhow::ensure!(
        dry_collection["items"]
            .as_array()
            .is_some_and(|items| {
                items.iter().any(|item| {
                    item.pointer("/metadata/name").and_then(Value::as_str)
                        == Some(name.as_str())
                })
            }),
        "dry-run collection delete did not return the matching ConfigMap: {dry_collection}"
    );
    anyhow::ensure!(
        configmaps.get(&name).await.is_ok(),
        "dry-run collection delete removed the ConfigMap"
    );
    let wrong = json!({"apiVersion": "v1", "kind": "DeleteOptions", "preconditions": {"resourceVersion": "0"}});
    let request = Request::builder()
        .method("DELETE")
        .uri(format!("/api/v1/namespaces/{}/configmaps/{}", context.namespace, name))
        .header("content-type", "application/json")
        .body(serde_json::to_vec(&wrong)?)?;
    match context.client.request::<Value>(request).await {
        Err(KubeError::Api(error)) if error.code == 409 => {}
        Err(error) => anyhow::bail!("wrong delete precondition returned the wrong API error: {error}"),
        Ok(value) => anyhow::bail!("wrong delete precondition unexpectedly deleted the object: {value}"),
    }
    anyhow::ensure!(configmaps.get(&name).await.is_ok(), "a failed delete precondition removed the object");
    configmaps.delete(&name, &DeleteParams::default()).await.context("cleaning up the delete-precondition fixture")?;
    anyhow::ensure!(!resource_version.is_empty(), "fixture resourceVersion was empty");
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

    let suffix = format!(
        "{}-{}",
        std::process::id(),
        Instant::now().elapsed().as_nanos()
    );
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
                // Let the apiserver select its configured default audience.
                // Hard-coding the in-cluster DNS name makes this bootstrap
                // fixture fail against distributions that configure a
                // different service audience.
                audiences: Vec::new(),
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
                let log = fs::read_to_string(&log_path).unwrap_or_else(|error| format!("<unreadable: {error}>"));
                anyhow::bail!(
                    "nodelet never submitted a TLS bootstrap CSR; log: {}\n{}",
                    log_path.display(),
                    log
                );
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
        // The output kubeconfig is the behavioral proof. The child remains
        // alive polling the issued CSR, so its line-buffered tracing output
        // is not necessarily visible until after it is stopped; requiring a
        // log line here made a successful bootstrap fail spuriously.
        Ok(())
    }
    .await;
    let _ = nodelet.kill();
    let _ = nodelet.wait();
    if let Some(csr_name) = csr_name_for_cleanup {
        let _ = csrs.delete(&csr_name, &DeleteParams::default()).await;
    }
    let _ = bindings
        .delete(&binding_name, &DeleteParams::default())
        .await;
    let _ = roles.delete(&role_name, &DeleteParams::default()).await;
    let _ = service_accounts
        .delete(&service_account_name, &DeleteParams::default())
        .await;
    let _ = fs::remove_dir_all(&scratch);
    result
}
