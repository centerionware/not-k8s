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
use k8s_openapi::api::core::v1::{ConfigMap, Endpoints, Pod, Service, ServiceAccount};
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
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

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
        run_privileged("mkdir", &[drop_in_dir.as_ref()])?;
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
        run_privileged("mkdir", &[drop_in_dir.as_ref()])?;
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

    let _override = NodeapiserverAuthenticationOverride::install()?;
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
        Err(KubeError::Api(error)) => anyhow::ensure!(
            error.code == 400,
            "nodeapiserver returned the wrong status for an unsupported field selector: {error}"
        ),
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
    let resource_version = created.metadata.resource_version.context("delete-precondition fixture had no resourceVersion")?;
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
