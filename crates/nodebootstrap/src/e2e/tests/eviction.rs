use super::context::E2eContext;
use super::skip_test;
use anyhow::Result;
use k8s_openapi::api::core::v1::Pod;
use kube::api::{Api, DeleteParams, PostParams};
use serde_json::json;
use std::time::Duration;

fn require_cri() -> Result<()> {
    anyhow::ensure!(
        crate::config::Config::from_env()?.nodelet_runtime() == "cri",
        "eviction checks require the CRI runtime",
    );
    Ok(())
}

async fn pod_is_evicted(pods: &Api<Pod>, name: &str) -> Result<bool> {
    Ok(pods
        .get(name)
        .await?
        .status
        .and_then(|status| status.reason)
        .as_deref()
        == Some("Evicted"))
}

async fn run_storage_eviction_case(
    context: &E2eContext,
    name: &str,
    volume: serde_json::Value,
    limit: Option<serde_json::Value>,
) -> Result<()> {
    require_cri().map_err(|error| skip_test(error.to_string()))?;
    let pods: Api<Pod> = Api::namespaced(context.client.clone(), &context.namespace);
    let mut container = json!({
        "name": "app",
        "image": "busybox:latest",
        "command": ["sh", "-c", "dd if=/dev/zero of=/data/bigfile bs=1M count=8 2>/dev/null; sleep 3600"],
        "volumeMounts": [{"name": "data", "mountPath": "/data"}]
    });
    if let Some(limit) = limit {
        container["resources"] = json!({"limits": {"ephemeral-storage": limit}});
    }
    let pod: Pod = serde_json::from_value(json!({
        "apiVersion": "v1",
        "kind": "Pod",
        "metadata": {"name": name},
        "spec": {
            "volumes": [{"name": "data", "emptyDir": volume}],
            "containers": [container]
        }
    }))?;
    pods.create(&PostParams::default(), &pod).await?;
    let result = context
        .wait_until(
            "Pod to be evicted for exceeding its ephemeral storage budget",
            Duration::from_secs(120),
            || {
                let pods = pods.clone();
                async move { pod_is_evicted(&pods, name).await }
            },
        )
        .await;
    let _ = pods.delete(name, &DeleteParams::default()).await;
    result
}

pub(super) async fn pod_exceeding_its_own_ephemeral_storage_limit_is_evicted(
    context: &E2eContext,
) -> Result<()> {
    run_storage_eviction_case(
        context,
        "ephemeral-storage-limit-check",
        json!({}),
        Some(json!("1Mi")),
    )
    .await
}

pub(super) async fn pod_exceeding_an_empty_dir_size_limit_is_evicted(
    context: &E2eContext,
) -> Result<()> {
    run_storage_eviction_case(
        context,
        "empty-dir-size-limit-check",
        json!({"sizeLimit": "1Mi"}),
        None,
    )
    .await
}

pub(super) async fn eviction_manual_procedure(_context: &E2eContext) -> Result<()> {
    Err(skip_test(
        "real node-pressure eviction is intentionally manual: use a temporary memory-pressure threshold and verify BestEffort eviction without exhausting the live node",
    ))
}

pub(super) async fn eviction_priority_tiebreak_manual_procedure(
    _context: &E2eContext,
) -> Result<()> {
    Err(skip_test(
        "priority-based eviction ordering requires deliberately induced node pressure and remains a manual live-cluster procedure",
    ))
}

pub(super) async fn eviction_exceeds_requests_tiebreak_manual_procedure(
    _context: &E2eContext,
) -> Result<()> {
    Err(skip_test(
        "exceeds-requests eviction ordering requires deliberately induced node pressure and remains a manual live-cluster procedure",
    ))
}

pub(super) async fn eviction_soft_grace_period_manual_procedure(
    _context: &E2eContext,
) -> Result<()> {
    Err(skip_test(
        "soft-threshold grace-period eviction requires sustained deliberately induced pressure and remains a manual live-cluster procedure",
    ))
}
