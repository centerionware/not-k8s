use super::context::{labels, E2eContext};
use anyhow::{Context, Result};
use k8s_openapi::api::apps::v1::Deployment;
use k8s_openapi::api::core::v1::Pod;
use kube::api::{Api, DeleteParams, ListParams, PostParams};
use serde_json::json;
use std::time::Duration;

pub(super) async fn garbage_collector_cascades_deployment_delete_to_replicaset_and_pods(
    context: &E2eContext,
) -> Result<()> {
    let name = "garbage-collector-test";
    let deployments: Api<Deployment> = Api::namespaced(context.client.clone(), &context.namespace);
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
        .context("creating garbage collector test Deployment")?;
    context
        .wait_until("garbage collector test Deployment creates a ReplicaSet", Duration::from_secs(60), || {
            let replicasets: Api<k8s_openapi::api::apps::v1::ReplicaSet> =
                Api::namespaced(context.client.clone(), &context.namespace);
            async move {
                Ok(replicasets
                    .list(&labels(&format!("app={name}")))
                    .await?
                    .items
                    .len()
                    == 1)
            }
        })
        .await?;
    context
        .wait_until("garbage collector test Deployment creates two Pods", Duration::from_secs(60), || {
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
    deployments.delete(name, &DeleteParams::default()).await?;
    context
        .wait_until("garbage collector removes the Deployment ReplicaSet", Duration::from_secs(60), || {
            let replicasets: Api<k8s_openapi::api::apps::v1::ReplicaSet> =
                Api::namespaced(context.client.clone(), &context.namespace);
            async move {
                Ok(replicasets
                    .list(&labels(&format!("app={name}")))
                    .await?
                    .items
                    .is_empty())
            }
        })
        .await?;
    context
        .wait_until("garbage collector removes the Deployment Pods", Duration::from_secs(120), || {
            let pods = pods.clone();
            async move {
                Ok(pods
                    .list(&ListParams::default().labels(&format!("app={name}")))
                    .await?
                    .items
                    .is_empty())
            }
        })
        .await
}
