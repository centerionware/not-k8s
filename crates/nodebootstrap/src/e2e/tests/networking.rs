use super::context::E2eContext;
use anyhow::{Context, Result};
use k8s_openapi::api::core::v1::{Node, Pod};
use kube::api::{Api, DeleteParams, ListParams, PostParams};
use serde_json::json;
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio::net::TcpStream;

async fn first_node(context: &E2eContext) -> Result<Node> {
    Api::<Node>::all(context.client.clone())
        .list(&ListParams::default())
        .await?
        .items
        .into_iter()
        .next()
        .context("the cluster has no Node object")
}

async fn pod_running(context: &E2eContext, name: &str) -> Result<bool> {
    let pods: Api<Pod> = Api::namespaced(context.client.clone(), &context.namespace);
    Ok(pods
        .get(name)
        .await?
        .status
        .and_then(|status| status.phase)
        .as_deref()
        == Some("Running"))
}

pub(super) async fn host_network_pod_uses_the_node_network_namespace(
    context: &E2eContext,
) -> Result<()> {
    let name = "host-network-pod";
    let pods: Api<Pod> = Api::namespaced(context.client.clone(), &context.namespace);
    let pod: Pod = serde_json::from_value(json!({
        "apiVersion": "v1",
        "kind": "Pod",
        "metadata": {"name": name},
        "spec": {
            "hostNetwork": true,
            "containers": [{"name": "app", "image": "busybox:latest", "command": ["sleep", "3600"]}]
        }
    }))?;
    pods.create(&PostParams::default(), &pod)
        .await
        .with_context(|| format!("creating Pod {name}"))?;
    context
        .wait_until("hostNetwork Pod Running", Duration::from_secs(90), || {
            pod_running(context, name)
        })
        .await?;

    let pod = pods.get(name).await?;
    let spec = pod.spec.context("hostNetwork Pod has no spec")?;
    anyhow::ensure!(spec.host_network == Some(true), "hostNetwork was not preserved");
    let node = first_node(context).await?;
    let node_ip = node
        .status
        .and_then(|status| status.addresses)
        .unwrap_or_default()
        .into_iter()
        .find(|address| address.type_ == "InternalIP")
        .map(|address| address.address)
        .context("the Node has no InternalIP")?;
    anyhow::ensure!(
        pod.status
            .and_then(|status| status.pod_ip)
            .as_deref()
            == Some(node_ip.as_str()),
        "hostNetwork Pod IP did not match the node InternalIP"
    );
    let _ = pods.delete(name, &DeleteParams::default()).await;
    Ok(())
}

pub(super) async fn host_port_reaches_the_container_on_the_node_ip(
    context: &E2eContext,
) -> Result<()> {
    let name = "host-port-pod";
    let host_port = 18080;
    let pods: Api<Pod> = Api::namespaced(context.client.clone(), &context.namespace);
    let pod: Pod = serde_json::from_value(json!({
        "apiVersion": "v1",
        "kind": "Pod",
        "metadata": {"name": name},
        "spec": {
            "containers": [{
                "name": "app",
                "image": "busybox:latest",
                "command": ["sh", "-c", "printf 'host-port-marker\\n' > /tmp/response; while true; do nc -l -p 8080 < /tmp/response; done"],
                "ports": [{"containerPort": 8080, "hostPort": host_port}]
            }]
        }
    }))?;
    pods.create(&PostParams::default(), &pod)
        .await
        .with_context(|| format!("creating Pod {name}"))?;
    context
        .wait_until("hostPort Pod Running", Duration::from_secs(90), || {
            pod_running(context, name)
        })
        .await?;

    let node = first_node(context).await?;
    let node_ip = node
        .status
        .and_then(|status| status.addresses)
        .unwrap_or_default()
        .into_iter()
        .find(|address| address.type_ == "InternalIP")
        .map(|address| address.address)
        .context("the Node has no InternalIP")?;
    let address = format!("{node_ip}:{host_port}");
    context
        .wait_until("hostPort to accept a connection", Duration::from_secs(60), || {
            let address = address.clone();
            async move {
                let Ok(Ok(mut stream)) = tokio::time::timeout(
                    Duration::from_secs(3),
                    TcpStream::connect(&address),
                )
                .await
                else {
                    return Ok(false);
                };
                let mut response = Vec::new();
                response.clear();
                stream.read_to_end(&mut response).await?;
                Ok(response
                    .windows(b"host-port-marker".len())
                    .any(|window| window == b"host-port-marker"))
            }
        })
        .await?;
    let _ = pods.delete(name, &DeleteParams::default()).await;
    Ok(())
}
