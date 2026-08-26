use super::context::{labels, E2eContext};
use anyhow::{Context, Result};
use k8s_openapi::api::apps::v1::ReplicaSet;
use k8s_openapi::api::core::v1::Pod;
use kube::api::{Api, Patch, PatchParams, PostParams};
use serde_json::json;
use std::time::Duration;

pub(super) async fn replicaset_creates_and_scales_pods(context: &E2eContext) -> Result<()> {
    let name = "replicaset-controller";
    let replicasets: Api<ReplicaSet> = Api::namespaced(context.client.clone(), &context.namespace);
    let pods: Api<Pod> = Api::namespaced(context.client.clone(), &context.namespace);
    let replicaset: ReplicaSet = serde_json::from_value(json!({
        "apiVersion": "apps/v1",
        "kind": "ReplicaSet",
        "metadata": {"name": name},
        "spec": {"replicas": 2, "selector": {"matchLabels": {"app": name}}, "template": {
            "metadata": {"labels": {"app": name}},
            "spec": {"containers": [{"name": "busybox", "image": "busybox:latest", "command": ["sleep", "3600"]}]}
        }}
    }))?;
    replicasets
        .create(&PostParams::default(), &replicaset)
        .await
        .context("creating ReplicaSet")?;
    context
        .wait_until("ReplicaSet to create two Pods", Duration::from_secs(60), || {
            let pods = pods.clone();
            async move {
                Ok(pods
                    .list(&labels(&format!("app={name}")))
                    .await?
                    .items
                    .len()
                    == 2)
            }
        })
        .await?;
    context
        .wait_until("ReplicaSet readyReplicas=2", Duration::from_secs(90), || {
            let replicasets = replicasets.clone();
            async move {
                Ok(replicasets
                    .get(name)
                    .await?
                    .status
                    .and_then(|status| status.ready_replicas)
                    == Some(2))
            }
        })
        .await?;
    let patch = json!({"spec": {"replicas": 1}});
    replicasets
        .patch(name, &PatchParams::default(), &Patch::Merge(&patch))
        .await
        .context("scaling ReplicaSet")?;
    context
        .wait_until(
            "ReplicaSet to scale down to one Pod",
            Duration::from_secs(60),
            || {
                let pods = pods.clone();
                async move {
                    Ok(pods
                        .list(&labels(&format!("app={name}")))
                        .await?
                        .items
                        .len()
                        == 1)
                }
            },
        )
        .await
}
