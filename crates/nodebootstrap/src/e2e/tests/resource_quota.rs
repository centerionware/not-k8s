use super::context::E2eContext;
use anyhow::{Context, Result};
use k8s_openapi::api::core::v1::{Pod, ResourceQuota};
use kube::api::{Api, DeleteParams, ListParams, PostParams};
use serde_json::json;
use std::time::Duration;

fn active_pod_count(pods: &[Pod]) -> usize {
    pods.iter()
        .filter(|pod| {
            !matches!(
                pod.status.as_ref().and_then(|status| status.phase.as_deref()),
                Some("Succeeded" | "Failed")
            )
        })
        .count()
}

pub(super) async fn resourcequota_used_pods_tracks_actual_pod_count(
    context: &E2eContext,
) -> Result<()> {
    let quota_name = "rq-test-quota";
    let pod_name = "rq-test-pod";
    let quotas: Api<ResourceQuota> = Api::namespaced(context.client.clone(), &context.namespace);
    let pods: Api<Pod> = Api::namespaced(context.client.clone(), &context.namespace);
    let quota: ResourceQuota = serde_json::from_value(json!({
        "apiVersion": "v1",
        "kind": "ResourceQuota",
        "metadata": {"name": quota_name},
        "spec": {"hard": {"pods": "100"}}
    }))?;
    quotas
        .create(&PostParams::default(), &quota)
        .await
        .context("creating ResourceQuota")?;
    let pod: Pod = serde_json::from_value(json!({
        "apiVersion": "v1",
        "kind": "Pod",
        "metadata": {"name": pod_name},
        "spec": {"containers": [{"name": "busybox", "image": "busybox:latest", "command": ["sleep", "3600"]}]}
    }))?;
    pods.create(&PostParams::default(), &pod)
        .await
        .context("creating ResourceQuota test Pod")?;
    context
        .wait_until(
            "ResourceQuota used.pods matches the active Pod count",
            Duration::from_secs(60),
            || {
                let quotas = quotas.clone();
                let pods = pods.clone();
                async move {
                    let quota = quotas.get(quota_name).await?;
                    let actual = active_pod_count(&pods.list(&ListParams::default()).await?.items);
                    let used = quota
                        .status
                        .and_then(|status| status.used)
                        .and_then(|used| used.get("pods").map(|quantity| quantity.0.clone()));
                    Ok(used == Some(actual.to_string()))
                }
            },
        )
        .await?;
    pods.delete(pod_name, &DeleteParams::default()).await?;
    context
        .wait_until("ResourceQuota test Pod is deleted", Duration::from_secs(60), || {
            let pods = pods.clone();
            async move { Ok(pods.get_opt(pod_name).await?.is_none()) }
        })
        .await?;
    context
        .wait_until(
            "ResourceQuota used.pods drops after Pod deletion",
            Duration::from_secs(60),
            || {
                let quotas = quotas.clone();
                let pods = pods.clone();
                async move {
                    let quota = quotas.get(quota_name).await?;
                    let actual = active_pod_count(&pods.list(&ListParams::default()).await?.items);
                    let used = quota
                        .status
                        .and_then(|status| status.used)
                        .and_then(|used| used.get("pods").map(|quantity| quantity.0.clone()))
                        .unwrap_or_else(|| "0".to_string());
                    Ok(used == actual.to_string())
                }
            },
        )
        .await?;
    let _ = quotas.delete(quota_name, &DeleteParams::default()).await;
    Ok(())
}
