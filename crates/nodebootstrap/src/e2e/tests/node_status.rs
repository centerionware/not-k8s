use super::context::E2eContext;
use anyhow::{Context, Result};
use k8s_openapi::api::core::v1::Node;
use kube::api::{Api, DeleteParams, ListParams, PostParams};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use std::time::Duration;

async fn e2e_node(context: &E2eContext) -> Result<Node> {
    Api::<Node>::all(context.client.clone())
        .list(&ListParams::default())
        .await?
        .items
        .into_iter()
        .next()
        .context("the cluster has no Node object")
}

pub(super) async fn node_is_ready_with_capacity_advertised(
    context: &E2eContext,
) -> Result<()> {
    let node = e2e_node(context).await?;
    let status = node.status.context("Node has no status")?;
    let ready = status
        .conditions
        .unwrap_or_default()
        .iter()
        .any(|condition| condition.type_ == "Ready" && condition.status == "True");
    anyhow::ensure!(ready, "the test Node did not report Ready=True");
    let capacity = status
        .capacity
        .as_ref()
        .context("Node.status.capacity is missing")?;
    let allocatable = status
        .allocatable
        .as_ref()
        .context("Node.status.allocatable is missing")?;
    for resource in ["cpu", "memory", "pods", "ephemeral-storage"] {
        anyhow::ensure!(
            capacity.contains_key(resource),
            "Node.status.capacity is missing {resource}"
        );
    }
    anyhow::ensure!(
        allocatable.get("ephemeral-storage") == capacity.get("ephemeral-storage"),
        "ephemeral-storage allocatable must equal capacity"
    );
    Ok(())
}

pub(super) async fn pressure_conditions_are_present(context: &E2eContext) -> Result<()> {
    let node = e2e_node(context).await?;
    let conditions = node
        .status
        .context("Node has no status")?
        .conditions
        .unwrap_or_default();
    for condition_type in ["MemoryPressure", "DiskPressure", "PIDPressure"] {
        anyhow::ensure!(
            conditions
                .iter()
                .any(|condition| condition.type_ == condition_type),
            "Node.status is missing {condition_type}"
        );
    }
    Ok(())
}

pub(super) async fn node_reports_real_kernel_and_os_image(
    context: &E2eContext,
) -> Result<()> {
    let node = e2e_node(context).await?;
    let info = node
        .status
        .context("Node has no status")?
        .node_info
        .context("Node has no nodeInfo")?;
    anyhow::ensure!(
        !info.kernel_version.is_empty() && info.kernel_version != "unknown",
        "Node.status.nodeInfo.kernelVersion is empty or a placeholder"
    );
    anyhow::ensure!(
        !info.os_image.is_empty(),
        "Node.status.nodeInfo.osImage is empty"
    );
    Ok(())
}

pub(super) async fn node_status_reports_runtime_handlers(context: &E2eContext) -> Result<()> {
    let node = e2e_node(context).await?;
    let handlers = node
        .status
        .context("Node has no status")?
        .runtime_handlers
        .unwrap_or_default();
    anyhow::ensure!(
        !handlers.is_empty(),
        "Node.status.runtimeHandlers is empty on a CRI node"
    );
    Ok(())
}

pub(super) async fn node_status_images_reflects_a_real_pulled_image(
    context: &E2eContext,
) -> Result<()> {
    let name = "node-status-images";
    let pods: Api<k8s_openapi::api::core::v1::Pod> =
        Api::namespaced(context.client.clone(), &context.namespace);
    let pod: k8s_openapi::api::core::v1::Pod = serde_json::from_value(serde_json::json!({
        "apiVersion": "v1",
        "kind": "Pod",
        "metadata": {"name": name},
        "spec": {"containers": [{"name": "app", "image": "busybox:latest", "command": ["sleep", "30"]}]}
    }))?;
    pods.create(&PostParams::default(), &pod).await?;
    context
        .wait_until("image Pod Running", Duration::from_secs(90), || {
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
        .await?;
    let nodes: Api<Node> = Api::all(context.client.clone());
    context
        .wait_until("Node.status.images busybox entry", Duration::from_secs(90), || {
            let nodes = nodes.clone();
            async move {
                Ok(nodes
                    .list(&ListParams::default())
                    .await?
                    .items
                    .into_iter()
                    .flat_map(|node| node.status.and_then(|status| status.images).unwrap_or_default())
                    .any(|image| {
                        image
                            .names
                            .iter()
                            .any(|name| name.as_str().contains("busybox"))
                            && image.size_bytes.is_some_and(|size| size > 0)
                    }))
            }
        })
        .await
}

pub(super) async fn node_gets_a_pod_cidr(context: &E2eContext) -> Result<()> {
    let nodes: Api<Node> = Api::all(context.client.clone());
    let suffix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    let name = format!("nodebootstrap-cidr-{}-{suffix}", std::process::id());
    nodes
        .create(
            &PostParams::default(),
            &Node {
                metadata: ObjectMeta {
                    name: Some(name.clone()),
                    ..Default::default()
                },
                ..Default::default()
            },
        )
        .await?;
    let result = context
        .wait_until("disposable Node.spec.podCIDR", Duration::from_secs(90), || {
            let nodes = nodes.clone();
            let name = name.clone();
            async move {
                Ok(nodes
                    .get(&name)
                    .await?
                    .spec
                    .and_then(|spec| spec.pod_cidr)
                    .is_some_and(|cidr| !cidr.is_empty()))
            }
        })
        .await;
    let _ = nodes.delete(&name, &DeleteParams::default()).await;
    result
}
