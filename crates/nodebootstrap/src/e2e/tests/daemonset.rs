use super::context::{labels, E2eContext};
use anyhow::{Context, Result};
use k8s_openapi::api::apps::v1::DaemonSet;
use k8s_openapi::api::core::v1::{Node, Pod};
use kube::api::{Api, DeleteParams, ListParams, PostParams};
use serde_json::json;
use std::time::Duration;

pub(super) async fn daemonset_places_a_pod_directly(context: &E2eContext) -> Result<()> {
    let name = "daemonset-controller";
    let daemonsets: Api<DaemonSet> = Api::namespaced(context.client.clone(), &context.namespace);
    let pods: Api<Pod> = Api::namespaced(context.client.clone(), &context.namespace);
    let nodes: Api<Node> = Api::all(context.client.clone());
    let node_name = nodes
        .list(&ListParams::default())
        .await?
        .items
        .into_iter()
        .next()
        .and_then(|node| node.metadata.name)
        .context("finding a node for the DaemonSet")?;
    let daemonset: DaemonSet = serde_json::from_value(json!({
        "apiVersion": "apps/v1",
        "kind": "DaemonSet",
        "metadata": {"name": name},
        "spec": {"selector": {"matchLabels": {"app": name}}, "template": {
            "metadata": {"labels": {"app": name}},
            "spec": {"containers": [{"name": "busybox", "image": "busybox:latest", "command": ["sleep", "3600"]}]}
        }}
    }))?;
    daemonsets
        .create(&PostParams::default(), &daemonset)
        .await
        .context("creating DaemonSet")?;
    context
        .wait_until(
            "DaemonSet Pod to receive the node name",
            Duration::from_secs(60),
            || {
                let pods = pods.clone();
                let node_name = node_name.clone();
                async move {
                    Ok(pods
                        .list(&labels(&format!("app={name}")))
                        .await?
                        .items
                        .into_iter()
                        .next()
                        .and_then(|pod| pod.spec.and_then(|spec| spec.node_name))
                        == Some(node_name))
                }
            },
        )
        .await?;
    context
        .wait_until("DaemonSet numberReady=1", Duration::from_secs(90), || {
            let daemonsets = daemonsets.clone();
            async move {
                Ok(daemonsets
                    .get(name)
                    .await?
                    .status
                    .is_some_and(|status| status.number_ready == 1))
            }
        })
        .await?;
    context
        .wait_until(
            "DaemonSet desiredNumberScheduled=1",
            Duration::from_secs(30),
            || {
                let daemonsets = daemonsets.clone();
                async move {
                    Ok(daemonsets
                        .get(name)
                        .await?
                        .status
                        .is_some_and(|status| status.desired_number_scheduled == 1))
                }
            },
        )
        .await?;
    let _ = daemonsets.delete(name, &DeleteParams::default()).await;
    Ok(())
}
