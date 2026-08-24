use super::context::E2eContext;
use anyhow::{Context, Result};
use k8s_openapi::api::core::v1::{Node, Pod, Service};
use k8s_openapi::api::discovery::v1::EndpointSlice;
use kube::api::{Api, ListParams, PostParams};
use serde_json::json;
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio::net::TcpStream;

async fn create_backend(context: &E2eContext, name: &str) -> Result<()> {
    let pods: Api<Pod> = Api::namespaced(context.client.clone(), &context.namespace);
    let pod: Pod = serde_json::from_value(json!({
        "apiVersion": "v1",
        "kind": "Pod",
        "metadata": {"name": name, "labels": {"app": name}},
        "spec": {"containers": [{"name": "app", "image": "busybox:latest", "command": ["sh", "-c", "while true; do printf 'service-proxy-marker\\n' | nc -l -p 8080; done"]}]}
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
    let Ok(Ok(mut stream)) = tokio::time::timeout(
        Duration::from_secs(3),
        TcpStream::connect(address),
    )
    .await
    else {
        return Ok(false);
    };
    let mut response = Vec::new();
    tokio::time::timeout(Duration::from_secs(3), stream.read_to_end(&mut response))
        .await
        .context("reading the service backend response")??;
    Ok(response
        .windows(b"service-proxy-marker".len())
        .any(|window| window == b"service-proxy-marker"))
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
    create_service_for_selector(
        context,
        "headless-service",
        "ClusterIP",
        18094,
        None,
        headless_backend,
    )
    .await?;
    let services: Api<Service> = Api::namespaced(context.client.clone(), &context.namespace);
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
