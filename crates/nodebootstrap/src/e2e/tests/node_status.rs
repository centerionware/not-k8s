use super::context::E2eContext;
use anyhow::{Context, Result};
use k8s_openapi::api::core::v1::Node;
use kube::api::{Api, ListParams};
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
    for resource in ["cpu", "memory", "pods", "ephemeral-storage"] {
        anyhow::ensure!(
            status.capacity.contains_key(resource),
            "Node.status.capacity is missing {resource}"
        );
    }
    anyhow::ensure!(
        status.allocatable.get("ephemeral-storage") == status.capacity.get("ephemeral-storage"),
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
        info.kernel_version
            .as_deref()
            .is_some_and(|value| !value.is_empty() && value != "unknown"),
        "Node.status.nodeInfo.kernelVersion is empty or a placeholder"
    );
    anyhow::ensure!(
        info.os_image.as_deref().is_some_and(|value| !value.is_empty()),
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

pub(super) async fn node_gets_a_pod_cidr(context: &E2eContext) -> Result<()> {
    let nodes: Api<Node> = Api::all(context.client.clone());
    context
        .wait_until("Node.spec.podCIDR", Duration::from_secs(90), || {
            let nodes = nodes.clone();
            async move {
                Ok(nodes
                    .list(&ListParams::default())
                    .await?
                    .items
                    .into_iter()
                    .any(|node| {
                        node.spec
                            .and_then(|spec| spec.pod_cidr)
                            .is_some_and(|cidr| !cidr.is_empty())
                    }))
            }
        })
        .await
}
