use super::context::E2eContext;
use anyhow::{Context, Result};
use k8s_openapi::api::core::v1::{Node, Pod, Service};
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
    let services: Api<Service> = Api::namespaced(context.client.clone(), &context.namespace);
    let mut service = json!({
        "apiVersion": "v1",
        "kind": "Service",
        "metadata": {"name": name},
        "spec": {"type": service_type, "selector": {"app": name}, "ports": [{"name": "http", "port": port, "targetPort": 8080}]}
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
