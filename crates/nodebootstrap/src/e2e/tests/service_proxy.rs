use super::context::E2eContext;
use super::skip_test;
use anyhow::{Context, Result};
use k8s_openapi::api::core::v1::{Node, Pod, Service};
use k8s_openapi::api::discovery::v1::EndpointSlice;
use k8s_openapi::api::rbac::v1::{Role, RoleBinding};
use kube::api::{Api, AttachParams, DeleteParams, ListParams, Patch, PatchParams, PostParams};
use serde_json::json;
use std::process::Command;
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio::net::TcpStream;

async fn create_backend(context: &E2eContext, name: &str) -> Result<()> {
    create_backend_with_marker(context, name, name, "service-proxy-marker").await
}

async fn exec_output(context: &E2eContext, pod: &str, command: &[&str]) -> Result<String> {
    exec_output_in(context, pod, "app", command).await
}

async fn exec_output_in(
    context: &E2eContext,
    pod: &str,
    container: &str,
    command: &[&str],
) -> Result<String> {
    let pods: Api<Pod> = Api::namespaced(context.client.clone(), &context.namespace);
    let params = AttachParams::default()
        .container(container)
        .stdout(true)
        .stderr(false);
    let mut process = pods.exec(pod, command.iter().copied(), &params).await?;
    let mut stdout = Vec::new();
    if let Some(mut stream) = process.stdout() {
        stream.read_to_end(&mut stdout).await?;
    }
    process.join().await?;
    Ok(String::from_utf8_lossy(&stdout).into_owned())
}

async fn create_backend_with_marker(
    context: &E2eContext,
    name: &str,
    selector: &str,
    marker: &str,
) -> Result<()> {
    let pods: Api<Pod> = Api::namespaced(context.client.clone(), &context.namespace);
    let pod: Pod = serde_json::from_value(json!({
        "apiVersion": "v1",
        "kind": "Pod",
        "metadata": {"name": name, "labels": {"app": selector}},
        "spec": {"containers": [{"name": "app", "image": "busybox:latest", "command": ["sh", "-c", format!("while true; do printf '{marker}\\n' | nc -l -p 8080; done")] }]}
    }))?;
    pods.create(&PostParams::default(), &pod).await?;
    context
        .wait_until("service backend Pod Running", Duration::from_secs(90), || {
            let pods = pods.clone();
            async move {
                Ok(pods
                    .get(name)
                    .await?
                    .status
                    .and_then(|status| status.phase)
                    .as_deref()
                    == Some("Running"))
            }
        })
        .await
}

async fn create_service(
    context: &E2eContext,
    name: &str,
    service_type: &str,
    port: i32,
    node_port: Option<i32>,
) -> Result<()> {
    create_service_for_selector(context, name, service_type, port, node_port, name).await
}

async fn create_service_for_selector(
    context: &E2eContext,
    name: &str,
    service_type: &str,
    port: i32,
    node_port: Option<i32>,
    selector: &str,
) -> Result<()> {
    let services: Api<Service> = Api::namespaced(context.client.clone(), &context.namespace);
    let mut service = json!({
        "apiVersion": "v1",
        "kind": "Service",
        "metadata": {"name": name},
        "spec": {"type": service_type, "selector": {"app": selector}, "ports": [{"name": "http", "port": port, "targetPort": 8080}]}
    });
    if let Some(node_port) = node_port {
        service["spec"]["ports"][0]["nodePort"] = json!(node_port);
    }
    let service: Service = serde_json::from_value(service)?;
    services.create(&PostParams::default(), &service).await?;
    Ok(())
}

async fn receives_marker(address: &str) -> Result<bool> {
    Ok(fetch_response(address)
        .await?
        .is_some_and(|response| response.contains("service-proxy-marker")))
}

async fn fetch_response(address: &str) -> Result<Option<String>> {
    let Ok(Ok(mut stream)) = tokio::time::timeout(
        Duration::from_secs(3),
        TcpStream::connect(address),
    )
    .await
    else {
        return Ok(None);
    };
    let mut response = Vec::new();
    tokio::time::timeout(Duration::from_secs(3), stream.read_to_end(&mut response))
        .await
        .context("reading the service backend response")??;
    Ok(Some(String::from_utf8_lossy(&response).into_owned()))
}

fn require_service_proxy() -> Result<()> {
    if crate::config::Config::from_env()?.nodelet_runtime() != "cri" {
        return Err(skip_test("service routing checks require the CRI runtime"));
    }
    // The runner is deliberately unprivileged while nodeproxy is a root
    // service. A successful process spawn is not enough here: `nft` returns
    // non-zero when the unprivileged probe cannot open netlink, so fall back
    // to sudo on both spawn failure and command failure. The old code only
    // used sudo for spawn failure and therefore skipped proxy tests even
    // while nodeproxy had successfully installed the table as root.
    let nft = match Command::new("nft")
        .args(["list", "table", "inet", "not_k8s_svc"])
        .output()
    {
        Ok(output) if output.status.success() => output,
        _ => match Command::new("sudo")
            .args(["nft", "list", "table", "inet", "not_k8s_svc"])
            .output()
        {
            Ok(output) => output,
            Err(_) => {
                return Err(skip_test(
                    "nodeproxy/nftables is unavailable; bootstrap with the proxy enabled and a usable nftables host",
                ))
            }
        },
    };
    if !nft.status.success() {
        return Err(skip_test(
            "nodeproxy/nftables is unavailable; bootstrap with the proxy enabled and a usable nftables host",
        ));
    }
    Ok(())
}

fn nft_table() -> Result<String> {
    let direct = Command::new("nft")
        .args(["list", "table", "inet", "not_k8s_svc"])
        .output();
    let output = match direct {
        Ok(output) if output.status.success() => output,
        _ => Command::new("sudo")
            .args(["nft", "list", "table", "inet", "not_k8s_svc"])
            .output()
            .context("reading the nodeproxy nftables table")?,
    };
    anyhow::ensure!(
        output.status.success(),
        "reading the nodeproxy nftables table failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn restart_nodeproxy() -> Result<()> {
    let uid = Command::new("id").arg("-u").output()?;
    let uid = String::from_utf8_lossy(&uid.stdout).trim().to_string();
    let output = if uid == "0" {
        Command::new("systemctl")
            .args(["restart", "nodeproxy.service"])
            .output()?
    } else {
        Command::new("sudo")
            .args(["systemctl", "restart", "nodeproxy.service"])
            .output()?
    };
    anyhow::ensure!(
        output.status.success(),
        "restarting nodeproxy.service failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(())
}

async fn service_cluster_ip(context: &E2eContext, name: &str) -> Result<String> {
    Api::<Service>::namespaced(context.client.clone(), &context.namespace)
        .get(name)
        .await?
        .spec
        .and_then(|spec| spec.cluster_ip)
        .filter(|ip| !ip.is_empty() && ip != "None")
        .context("Service did not receive a ClusterIP")
}

async fn terminated_message(context: &E2eContext, name: &str) -> Result<Option<String>> {
    let pods: Api<Pod> = Api::namespaced(context.client.clone(), &context.namespace);
    Ok(pods
        .get(name)
        .await?
        .status
        .and_then(|status| status.container_statuses)
        .unwrap_or_default()
        .into_iter()
        .find(|status| status.name == "app")
        .and_then(|status| status.state)
        .and_then(|state| state.terminated)
        .and_then(|terminated| terminated.message))
}

async fn node_internal_ip(context: &E2eContext) -> Result<String> {
    Api::<Node>::all(context.client.clone())
        .list(&ListParams::default())
        .await?
        .items
        .into_iter()
        .next()
        .context("the cluster has no Node object")?
        .status
        .and_then(|status| status.addresses)
        .unwrap_or_default()
        .into_iter()
        .find(|address| address.type_ == "InternalIP")
        .map(|address| address.address)
        .context("the Node has no InternalIP")
}

pub(super) async fn clusterip_service_routes_to_its_backend_pod(
    context: &E2eContext,
) -> Result<()> {
    let name = "clusterip-routing";
    create_backend(context, name).await?;
    create_service(context, name, "ClusterIP", 18090, None).await?;
    let services: Api<Service> = Api::namespaced(context.client.clone(), &context.namespace);
    context
        .wait_until("ClusterIP service to route", Duration::from_secs(90), || {
            let services = services.clone();
            async move {
                let cluster_ip = services
                    .get(name)
                    .await?
                    .spec
                    .and_then(|spec| spec.cluster_ip)
                    .filter(|ip| !ip.is_empty());
                let Some(cluster_ip) = cluster_ip else {
                    return Ok(false);
                };
                receives_marker(&format!("{cluster_ip}:18090")).await
            }
        })
        .await
}

pub(super) async fn nodeport_service_is_reachable_on_the_node_ip(
    context: &E2eContext,
) -> Result<()> {
    let name = "nodeport-routing";
    create_backend(context, name).await?;
    create_service(context, name, "NodePort", 18091, Some(30080)).await?;
    let node_ip = node_internal_ip(context).await?;
    context
        .wait_until("NodePort service to route", Duration::from_secs(90), || {
            let address = format!("{node_ip}:30080");
            async move { receives_marker(&address).await }
        })
        .await
}

pub(super) async fn service_with_no_endpoints_does_not_wedge_the_ruleset(
    context: &E2eContext,
) -> Result<()> {
    let name = "service-without-endpoints";
    create_service(context, name, "ClusterIP", 18092, None).await?;
    let slices: Api<EndpointSlice> = Api::namespaced(context.client.clone(), &context.namespace);
    context
        .wait_until("empty EndpointSlice for a service without backends", Duration::from_secs(60), || {
            let slices = slices.clone();
            async move {
                let items = slices
                    .list(&ListParams::default().labels(&format!("kubernetes.io/service-name={name}")))
                    .await?
                    .items;
                Ok(!items.is_empty() && items.iter().all(|slice| slice.endpoints.is_empty()))
            }
        })
        .await
}

pub(super) async fn headless_service_does_not_break_other_services(
    context: &E2eContext,
) -> Result<()> {
    let headless_backend = "headless-backend";
    create_backend(context, headless_backend).await?;
    let services: Api<Service> = Api::namespaced(context.client.clone(), &context.namespace);
    let headless: Service = serde_json::from_value(json!({
        "apiVersion": "v1",
        "kind": "Service",
        "metadata": {"name": "headless-service"},
        "spec": {
            "clusterIP": "None",
            "selector": {"app": headless_backend},
            "ports": [{"name": "http", "port": 18094, "targetPort": 8080}]
        }
    }))?;
    services.create(&PostParams::default(), &headless).await?;
    let cluster_ip = services
        .get("headless-service")
        .await?
        .spec
        .and_then(|spec| spec.cluster_ip);
    anyhow::ensure!(
        cluster_ip.as_deref() == Some("None"),
        "headless Service did not receive clusterIP=None: {cluster_ip:?}"
    );
    let slices: Api<EndpointSlice> = Api::namespaced(context.client.clone(), &context.namespace);
    context
        .wait_until("headless Service EndpointSlice", Duration::from_secs(60), || {
            let slices = slices.clone();
            async move {
                Ok(slices
                    .list(&ListParams::default().labels(&format!(
                        "kubernetes.io/service-name=headless-service"
                    )))
                    .await?
                    .items
                    .iter()
                    .any(|slice| {
                        !slice.endpoints.is_empty()
                            && slice
                                .endpoints
                                .iter()
                                .any(|endpoint| !endpoint.addresses.is_empty())
                    }))
            }
        })
        .await?;
    anyhow::ensure!(
        !nft_table()?.contains("None"),
        "headless Service's clusterIP=None reached the nodeproxy nftables ruleset"
    );

    let probe = "headless-probe";
    create_backend(context, probe).await?;
    create_service(context, probe, "ClusterIP", 18095, None).await?;
    context
        .wait_until("normal Service beside headless Service", Duration::from_secs(90), || {
            let services = services.clone();
            async move {
                let cluster_ip = services
                    .get(probe)
                    .await?
                    .spec
                    .and_then(|spec| spec.cluster_ip)
                    .filter(|ip| !ip.is_empty());
                let Some(cluster_ip) = cluster_ip else {
                    return Ok(false);
                };
                receives_marker(&format!("{cluster_ip}:18095")).await
            }
        })
        .await
}

pub(super) async fn clusterip_is_reachable_from_inside_a_pod(
    context: &E2eContext,
) -> Result<()> {
    let name = "clusterip-inside";
    create_backend(context, name).await?;
    create_service(context, name, "ClusterIP", 18093, None).await?;
    let services: Api<Service> = Api::namespaced(context.client.clone(), &context.namespace);
    let cluster_ip = services
        .get(name)
        .await?
        .spec
        .and_then(|spec| spec.cluster_ip)
        .filter(|ip| !ip.is_empty())
        .context("ClusterIP service did not receive a cluster IP")?;
    let client_name = "clusterip-inside-client";
    let pods: Api<Pod> = Api::namespaced(context.client.clone(), &context.namespace);
    let client: Pod = serde_json::from_value(json!({
        "apiVersion": "v1",
        "kind": "Pod",
        "metadata": {"name": client_name},
        "spec": {
            "restartPolicy": "Never",
            "containers": [{"name": "app", "image": "busybox:latest", "command": ["sh", "-c", format!("wget -qO- --timeout=5 http://{cluster_ip}:18093/ > /dev/termination-log")]}]
        }
    }))?;
    pods.create(&PostParams::default(), &client).await?;
    context
        .wait_until("ClusterIP access from a Pod", Duration::from_secs(90), || {
            let context = context.clone();
            async move {
                Ok(terminated_message(&context, client_name)
                    .await?
                    .is_some_and(|message| message.contains("service-proxy-marker")))
            }
        })
        .await
}

pub(super) async fn nodeproxy_runs_as_its_own_service_separate_from_nodelet(
    _context: &E2eContext,
) -> Result<()> {
    require_service_proxy()?;
    if Command::new("systemctl")
        .args(["list-unit-files", "nodeproxy.service"])
        .output()
        .is_err()
    {
        return Err(skip_test("nodeproxy service checks require systemd"));
    }
    let unit = Command::new("systemctl")
        .args(["cat", "nodeproxy.service"])
        .output()?;
    if !unit.status.success() {
        return Err(skip_test(
            "nodeproxy.service is not installed; bootstrap with the proxy enabled",
        ));
    }
    let text = String::from_utf8_lossy(&unit.stdout);
    let ordering = text
        .lines()
        .filter(|line| {
            ["After=", "Before=", "Wants=", "Requires=", "Requisite=", "BindsTo=", "PartOf="]
                .iter()
                .any(|prefix| line.starts_with(prefix))
        })
        .collect::<Vec<_>>();
    anyhow::ensure!(!ordering.is_empty(), "nodeproxy.service has no ordering directives");
    anyhow::ensure!(
        ordering.iter().all(|line| !line.contains("nodelet")),
        "nodeproxy.service must not order against nodelet.service: {ordering:?}"
    );
    let exec_start = text
        .lines()
        .find(|line| line.starts_with("ExecStart="))
        .context("nodeproxy.service has no ExecStart")?;
    anyhow::ensure!(
        exec_start.contains("nodeproxy"),
        "nodeproxy.service ExecStart does not name nodeproxy: {exec_start}"
    );
    Ok(())
}

pub(super) async fn a_pod_reaching_its_own_service_gets_hairpin_masquerade(
    context: &E2eContext,
) -> Result<()> {
    require_service_proxy()?;
    let name = "service-hairpin";
    create_backend_with_marker(context, name, name, "hairpin-marker").await?;
    create_service(context, name, "ClusterIP", 18100, None).await?;
    let slices: Api<EndpointSlice> = Api::namespaced(context.client.clone(), &context.namespace);
    context
        .wait_until("hairpin Service endpoint", Duration::from_secs(90), || {
            let slices = slices.clone();
            async move { Ok(ready_endpoint_count_from_api(&slices, name).await? > 0) }
        })
        .await?;
    let cluster_ip = service_cluster_ip(context, name).await?;
    anyhow::ensure!(
        nft_table()?.contains("masquerade"),
        "nodeproxy nftables table has no hairpin masquerade rule"
    );
        context
        .wait_until("hairpin request to the backend's own Service", Duration::from_secs(60), || {
            let context = context.clone();
            let cluster_ip = cluster_ip.clone();
            async move {
                let url = format!("http://{cluster_ip}:18100/");
                let output = exec_output(
                    &context,
                    name,
                    &["wget", "-qO-", "--timeout=5", url.as_str()],
                )
                .await;
                Ok(output.is_ok_and(|output| output.contains("hairpin-marker")))
            }
        })
        .await
}

async fn ready_endpoint_count_from_api(
    slices: &Api<EndpointSlice>,
    service: &str,
) -> Result<usize> {
    Ok(slices
        .list(&ListParams::default().labels(&format!(
            "kubernetes.io/service-name={service}"
        )))
        .await?
        .items
        .iter()
        .flat_map(|slice| &slice.endpoints)
        .filter(|endpoint| endpoint.conditions.as_ref().map_or(true, |conditions| {
            conditions.ready != Some(false)
        }))
        .filter(|endpoint| !endpoint.addresses.is_empty())
        .count())
}

pub(super) async fn multiple_backends_actually_share_traffic(
    context: &E2eContext,
) -> Result<()> {
    require_service_proxy()?;
    let service = "service-multiple-backends";
    create_backend_with_marker(context, "service-backend-a", service, "multibackend-a").await?;
    create_backend_with_marker(context, "service-backend-b", service, "multibackend-b").await?;
    create_service(context, service, "ClusterIP", 18101, None).await?;
    let slices: Api<EndpointSlice> = Api::namespaced(context.client.clone(), &context.namespace);
    context
        .wait_until("two ready Service endpoints", Duration::from_secs(90), || {
            let slices = slices.clone();
            async move { Ok(ready_endpoint_count_from_api(&slices, service).await? >= 2) }
        })
        .await?;
    let cluster_ip = service_cluster_ip(context, service).await?;
    let mut seen_a = false;
    let mut seen_b = false;
    for _ in 0..40 {
        if let Some(response) = fetch_response(&format!("{cluster_ip}:18101")).await? {
            seen_a |= response.contains("multibackend-a");
            seen_b |= response.contains("multibackend-b");
        }
        if seen_a && seen_b {
            break;
        }
    }
    anyhow::ensure!(
        seen_a && seen_b,
        "two-backend Service did not reach both backends (a={seen_a}, b={seen_b})"
    );
    Ok(())
}

pub(super) async fn losing_every_backend_removes_the_dnat_rule(
    context: &E2eContext,
) -> Result<()> {
    require_service_proxy()?;
    let service = "service-drain";
    let backend = "service-drain-backend";
    create_backend_with_marker(context, backend, service, "drain-marker").await?;
    create_service(context, service, "ClusterIP", 18102, None).await?;
    let slices: Api<EndpointSlice> = Api::namespaced(context.client.clone(), &context.namespace);
    context
        .wait_until("drain Service endpoint", Duration::from_secs(90), || {
            let slices = slices.clone();
            async move { Ok(ready_endpoint_count_from_api(&slices, service).await? > 0) }
        })
        .await?;
    let cluster_ip = service_cluster_ip(context, service).await?;
    context
        .wait_until("drain Service nftables rule", Duration::from_secs(60), || async {
            Ok(nft_table()?.contains(&cluster_ip))
        })
        .await?;
    let pods: Api<Pod> = Api::namespaced(context.client.clone(), &context.namespace);
    pods.delete(backend, &DeleteParams::default()).await?;
    context
        .wait_until("drained Service nftables rule to disappear", Duration::from_secs(90), || async {
            Ok(!nft_table()?.contains(&cluster_ip))
        })
        .await
}

pub(super) async fn deleting_a_service_removes_its_rules(
    context: &E2eContext,
) -> Result<()> {
    require_service_proxy()?;
    let service = "service-delete-rules";
    create_backend_with_marker(context, "service-delete-backend", service, "delete-marker").await?;
    create_service(context, service, "ClusterIP", 18103, None).await?;
    let slices: Api<EndpointSlice> = Api::namespaced(context.client.clone(), &context.namespace);
    context
        .wait_until("deletable Service endpoint", Duration::from_secs(90), || {
            let slices = slices.clone();
            async move { Ok(ready_endpoint_count_from_api(&slices, service).await? > 0) }
        })
        .await?;
    let cluster_ip = service_cluster_ip(context, service).await?;
    context
        .wait_until("deletable Service nftables rule", Duration::from_secs(60), || async {
            Ok(nft_table()?.contains(&cluster_ip))
        })
        .await?;
    let services: Api<Service> = Api::namespaced(context.client.clone(), &context.namespace);
    services.delete(service, &DeleteParams::default()).await?;
    context
        .wait_until("deleted Service nftables rule to disappear", Duration::from_secs(90), || async {
            Ok(!nft_table()?.contains(&cluster_ip))
        })
        .await
}

pub(super) async fn session_affinity_pins_a_client_to_one_backend(
    context: &E2eContext,
) -> Result<()> {
    require_service_proxy()?;
    let service = "service-affinity";
    create_backend_with_marker(context, "service-affinity-a", service, "affinity-a").await?;
    create_backend_with_marker(context, "service-affinity-b", service, "affinity-b").await?;
    create_service(context, service, "ClusterIP", 18104, None).await?;
    let services: Api<Service> = Api::namespaced(context.client.clone(), &context.namespace);
    services
        .patch(
            service,
            &PatchParams::default(),
            &Patch::Merge(json!({"spec": {"sessionAffinity": "ClientIP"}})),
        )
        .await?;
    let slices: Api<EndpointSlice> = Api::namespaced(context.client.clone(), &context.namespace);
    context
        .wait_until("session-affinity Service endpoints", Duration::from_secs(90), || {
            let slices = slices.clone();
            async move { Ok(ready_endpoint_count_from_api(&slices, service).await? >= 2) }
        })
        .await?;
    let cluster_ip = service_cluster_ip(context, service).await?;
    let mut first = None;
    for _ in 0..20 {
        let Some(response) = fetch_response(&format!("{cluster_ip}:18104")).await? else {
            continue;
        };
        let marker = if response.contains("affinity-a") {
            "affinity-a"
        } else if response.contains("affinity-b") {
            "affinity-b"
        } else {
            continue;
        };
        if let Some(expected) = first {
            anyhow::ensure!(expected == marker, "ClientIP session affinity changed backend");
        } else {
            first = Some(marker);
        }
    }
    anyhow::ensure!(first.is_some(), "session-affinity Service never returned a backend");
    Ok(())
}

pub(super) async fn nodeproxy_rebuilds_the_whole_ruleset_after_a_restart(
    context: &E2eContext,
) -> Result<()> {
    require_service_proxy()?;
    if !Command::new("systemctl")
        .args(["list-unit-files", "nodeproxy.service"])
        .status()
        .is_ok_and(|status| status.success())
    {
        return Err(skip_test("nodeproxy restart checks require systemd"));
    }
    let service = "service-restart-rebuild";
    create_backend_with_marker(context, "service-restart-backend", service, "restart-marker").await?;
    create_service(context, service, "ClusterIP", 18105, None).await?;
    let slices: Api<EndpointSlice> = Api::namespaced(context.client.clone(), &context.namespace);
    context
        .wait_until("restart-check Service endpoint", Duration::from_secs(90), || {
            let slices = slices.clone();
            async move { Ok(ready_endpoint_count_from_api(&slices, service).await? > 0) }
        })
        .await?;
    let cluster_ip = service_cluster_ip(context, service).await?;
    context
        .wait_until("restart-check Service before restart", Duration::from_secs(60), || async {
            Ok(receives_marker(&format!("{cluster_ip}:18105")).await?)
        })
        .await?;
    restart_nodeproxy()?;
    context
        .wait_until("restart-check Service after nodeproxy restart", Duration::from_secs(90), || async {
            Ok(receives_marker(&format!("{cluster_ip}:18105")).await?)
        })
        .await
}

pub(super) async fn a_long_lived_watch_survives_a_service_churn_burst(
    context: &E2eContext,
) -> Result<()> {
    require_service_proxy()?;
    let role_name = "established-conn-watcher-pods";
    let roles: Api<Role> = Api::namespaced(context.client.clone(), &context.namespace);
    let bindings: Api<RoleBinding> =
        Api::namespaced(context.client.clone(), &context.namespace);
    let role: Role = serde_json::from_value(json!({
        "apiVersion": "rbac.authorization.k8s.io/v1",
        "kind": "Role",
        "metadata": {"name": role_name},
        "rules": [{"apiGroups": [""], "resources": ["pods"], "verbs": ["get", "list", "watch"]}]
    }))?;
    let binding: RoleBinding = serde_json::from_value(json!({
        "apiVersion": "rbac.authorization.k8s.io/v1",
        "kind": "RoleBinding",
        "metadata": {"name": role_name},
        "subjects": [{"kind": "ServiceAccount", "name": "default", "namespace": context.namespace}],
        "roleRef": {"kind": "Role", "name": role_name, "apiGroup": "rbac.authorization.k8s.io"}
    }))?;
    roles.create(&PostParams::default(), &role).await?;
    bindings.create(&PostParams::default(), &binding).await?;

    let pod_name = "established-conn-watcher";
    let pods: Api<Pod> = Api::namespaced(context.client.clone(), &context.namespace);
    let pod: Pod = serde_json::from_value(json!({
        "apiVersion": "v1",
        "kind": "Pod",
        "metadata": {"name": pod_name},
        "spec": {
            "serviceAccountName": "default",
            "containers": [{
                "name": "watcher",
                "image": "curlimages/curl:8.10.1",
                "command": ["sh", "-c", format!(
                    "TOKEN=$(cat /var/run/secrets/kubernetes.io/serviceaccount/token); CACERT=/var/run/secrets/kubernetes.io/serviceaccount/ca.crt; curl -sS -N --connect-timeout 5 --max-time 90 --cacert $CACERT -H \"Authorization: Bearer $TOKEN\" \"https://$KUBERNETES_SERVICE_HOST:$KUBERNETES_SERVICE_PORT/api/v1/namespaces/{}/pods?watch=true&timeoutSeconds=85\" > /tmp/watch.out 2>/tmp/watch.err & echo $! > /tmp/watch.pid; wait $(cat /tmp/watch.pid); echo WATCH_EXIT=$? >> /tmp/watch.err; sleep 3600",
                    context.namespace
                )]
            }]
        }
    }))?;
    pods.create(&PostParams::default(), &pod).await?;
    context
        .wait_until("long-lived watch Pod to reach Running", Duration::from_secs(120), || {
            let pods = pods.clone();
            async move {
                Ok(pods
                    .get(pod_name)
                    .await?
                    .status
                    .and_then(|status| status.phase)
                    .as_deref()
                    == Some("Running"))
            }
        })
        .await?;

    context
        .wait_until("watch Pod to establish its API connection", Duration::from_secs(45), || {
            let context = context.clone();
            async move {
                let bytes = exec_output_in(&context, pod_name, "watcher", &["sh", "-c", "wc -c < /tmp/watch.out"]).await
                    .unwrap_or_default();
                let alive = exec_output_in(&context, pod_name, "watcher", &["sh", "-c", "kill -0 $(cat /tmp/watch.pid)"]).await
                    .is_ok();
                Ok(bytes.trim().parse::<u64>().unwrap_or_default() > 0 && alive)
            }
        })
        .await?;

    for index in 1..=25 {
        create_service_for_selector(
            context,
            &format!("churn-svc-{index}"),
            "ClusterIP",
            80,
            None,
            &format!("churn-svc-{index}-nonexistent"),
        )
        .await?;
    }
    tokio::time::sleep(Duration::from_secs(20)).await;

    let bytes = exec_output_in(
        context,
        pod_name,
        "watcher",
        &["sh", "-c", "wc -c < /tmp/watch.out"],
    )
    .await?;
    let alive = exec_output_in(
        context,
        pod_name,
        "watcher",
        &["sh", "-c", "kill -0 $(cat /tmp/watch.pid)"],
    )
    .await
    .is_ok();
    let error_log = exec_output_in(
        context,
        pod_name,
        "watcher",
        &["sh", "-c", "cat /tmp/watch.err"],
    )
    .await
    .unwrap_or_default();
    anyhow::ensure!(
        bytes.trim().parse::<u64>().unwrap_or_default() > 0,
        "long-lived watch received no bytes after Service churn"
    );
    anyhow::ensure!(
        alive,
        "curl exited during Service churn; watch error log: {error_log}"
    );
    anyhow::ensure!(
        !error_log.contains("connection reset by peer"),
        "Service churn reset the long-lived pod-to-apiserver connection: {error_log}"
    );
    Ok(())
}
