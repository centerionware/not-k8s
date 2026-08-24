use super::context::{labels, E2eContext};
use anyhow::{Context, Result};
use k8s_openapi::api::apps::v1::{Deployment, ReplicaSet};
use k8s_openapi::api::core::v1::Pod;
use kube::api::{Api, Patch, PatchParams, PostParams};
use serde_json::json;
use std::time::Duration;

pub(super) async fn deployment_creates_replicaset_and_rolls_update(
    context: &E2eContext,
) -> Result<()> {
    let name = "deployment-controller";
    let deployments: Api<Deployment> = Api::namespaced(context.client.clone(), &context.namespace);
    let replicasets: Api<ReplicaSet> = Api::namespaced(context.client.clone(), &context.namespace);
    let pods: Api<Pod> = Api::namespaced(context.client.clone(), &context.namespace);
    let deployment: Deployment = serde_json::from_value(json!({
        "apiVersion": "apps/v1",
        "kind": "Deployment",
        "metadata": {"name": name},
        "spec": {"replicas": 2, "selector": {"matchLabels": {"app": name}}, "template": {
            "metadata": {"labels": {"app": name}},
            "spec": {"containers": [{"name": "busybox", "image": "busybox:latest", "command": ["sleep", "3600"]}]}
        }}
    }))?;
    deployments
        .create(&PostParams::default(), &deployment)
        .await
        .context("creating Deployment")?;
    context
        .wait_until(
            "Deployment to create one ReplicaSet",
            Duration::from_secs(60),
            || {
                let replicasets = replicasets.clone();
                async move {
                    Ok(replicasets
                        .list(&labels(&format!("app={name}")))
                        .await?
                        .items
                        .len()
                        == 1)
                }
            },
        )
        .await?;
    context
        .wait_until("Deployment to create two Pods", Duration::from_secs(60), || {
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
        .wait_until("Deployment readyReplicas=2", Duration::from_secs(90), || {
            let deployments = deployments.clone();
            async move {
                Ok(deployments
                    .get(name)
                    .await?
                    .status
                    .and_then(|status| status.ready_replicas)
                    == Some(2))
            }
        })
        .await?;
    let patch = json!({
        "spec": {"template": {"spec": {"containers": [{
            "name": "busybox", "image": "busybox:latest", "command": ["sleep", "7200"]
        }]}}}
    });
    deployments
        .patch(name, &PatchParams::default(), &Patch::Merge(&patch))
        .await
        .context("patching Deployment template")?;
    context
        .wait_until(
            "Deployment to create a second ReplicaSet",
            Duration::from_secs(90),
            || {
                let replicasets = replicasets.clone();
                async move {
                    Ok(replicasets
                        .list(&labels(&format!("app={name}")))
                        .await?
                        .items
                        .len()
                        >= 2)
                }
            },
        )
        .await?;
    context
        .wait_until(
            "Deployment to retain two Pods after rollout",
            Duration::from_secs(90),
            || {
                let pods = pods.clone();
                async move {
                    Ok(pods
                        .list(&labels(&format!("app={name}")))
                        .await?
                        .items
                        .len()
                        == 2)
                }
            },
        )
        .await
}
