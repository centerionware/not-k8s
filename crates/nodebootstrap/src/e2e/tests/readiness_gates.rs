use super::context::E2eContext;
use anyhow::{Context, Result};
use k8s_openapi::api::core::v1::Pod;
use kube::api::{Api, DeleteParams, Patch, PatchParams, PostParams};
use serde_json::json;
use std::time::Duration;

fn condition_status(pod: &Pod, condition_type: &str) -> Option<String> {
    pod.status
        .as_ref()
        .and_then(|status| status.conditions.as_ref())
        .into_iter()
        .flatten()
        .find(|condition| condition.type_ == condition_type)
        .map(|condition| condition.status.clone())
}

pub(super) async fn pod_stays_not_ready_until_its_readiness_gate_condition_is_set(
    context: &E2eContext,
) -> Result<()> {
    let name = "readiness-gate-check";
    let gate = "www.example.com/feature-flag";
    let pods: Api<Pod> = Api::namespaced(context.client.clone(), &context.namespace);
    let pod: Pod = serde_json::from_value(json!({
        "apiVersion": "v1",
        "kind": "Pod",
        "metadata": {"name": name},
        "spec": {
            "readinessGates": [{"conditionType": gate}],
            "containers": [{"name": "app", "image": "busybox:latest", "command": ["sleep", "3600"]}]
        }
    }))?;
    pods.create(&PostParams::default(), &pod)
        .await
        .context("creating readiness-gated Pod")?;
    context
        .wait_until("readiness-gated Pod Running", Duration::from_secs(90), || {
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
    context
        .wait_until("readiness-gated Pod ContainersReady", Duration::from_secs(30), || {
            let pods = pods.clone();
            async move {
                Ok(condition_status(&pods.get(name).await?, "ContainersReady").as_deref() == Some("True"))
            }
        })
        .await?;
    anyhow::ensure!(
        condition_status(&pods.get(name).await?, "Ready").as_deref() == Some("False"),
        "Ready must be False while the readiness gate is unset"
    );

    // Ordinary PATCH and /status contend on the same stored Pod. Verify the
    // ordinary handler also retries its internal CAS without losing fields.
    for round in 0..4 {
        let mut writers = tokio::task::JoinSet::new();
        let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(4));
        for index in 0..4 {
            let pods = pods.clone();
            let barrier = barrier.clone();
            writers.spawn(async move {
                let key = format!("e2e.not-k8s.io/writer-{round}-{index}");
                let patch = json!({"metadata":{"annotations":{(key):"kept"}}});
                barrier.wait().await;
                pods.patch(name, &PatchParams::default(), &Patch::Merge(&patch)).await
            });
        }
        while let Some(result) = writers.join_next().await {
            result.context("joining concurrent metadata writer")?
                .context("unconditional metadata PATCH must absorb internal CAS conflicts")?;
        }
        context.wait_until("all concurrent metadata patches remain visible", Duration::from_secs(10), || {
            let pods = pods.clone();
            async move {
                let annotations = pods.get(name).await?.metadata.annotations.unwrap_or_default();
                Ok((0..4).all(|index| annotations.get(&format!("e2e.not-k8s.io/writer-{round}-{index}"))
                    .is_some_and(|value| value == "kept")))
            }
        }).await?;
    }

    let first = pods.patch(name, &PatchParams::default(), &Patch::Merge(&json!({
        "metadata":{"annotations":{"e2e.not-k8s.io/version-check":"first"}}
    }))).await?;
    pods.patch(name, &PatchParams::default(), &Patch::Merge(&json!({
        "metadata":{"annotations":{"e2e.not-k8s.io/version-check":"second"}}
    }))).await?;
    let conditional = json!({"metadata":{"resourceVersion":first.metadata.resource_version,
        "annotations":{"e2e.not-k8s.io/version-check":"must-not-write"}}});
    let rejected = pods.patch(name, &PatchParams::default(), &Patch::Merge(&conditional)).await;
    anyhow::ensure!(matches!(rejected, Err(kube::Error::Api(ref response)) if response.code == 409),
        "ordinary PATCH must reject an explicit stale resourceVersion");

    // Exercise independent status writers racing the real nodelet. Strategic
    // merge must preserve every condition and absorb internal storage CAS
    // races without asking unconditional PATCH callers to retry themselves.
    for round in 0..4 {
        let mut writers = tokio::task::JoinSet::new();
        let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(4));
        for index in 0..4 {
            let pods = pods.clone();
            let barrier = barrier.clone();
            writers.spawn(async move {
                let patch = json!({"status":{"conditions":[{
                    "type":format!("www.example.com/writer-{round}-{index}"), "status":"True"
                }]}});
                barrier.wait().await;
                pods.patch_status(name, &PatchParams::default(), &Patch::Strategic(&patch)).await
            });
        }
        while let Some(result) = writers.join_next().await {
            result.context("joining concurrent status writer")?
                .context("unconditional concurrent status PATCH must absorb internal CAS conflicts")?;
        }
        let current = pods.get(name).await?;
        for index in 0..4 {
            anyhow::ensure!(condition_status(&current, &format!("www.example.com/writer-{round}-{index}"))
                .as_deref() == Some("True"), "concurrent status condition was lost");
        }
    }
    let stale_version = pods.get(name).await?.metadata.resource_version;
    let patch = json!({"status": {"conditions": [{"type": gate, "status": "False"}]}});
    pods.patch_status(name, &PatchParams::default(), &Patch::Merge(&patch))
        .await
        .context("clearing readiness gate condition")?;
    let conditional = json!({"metadata":{"resourceVersion":stale_version},
        "status":{"conditions":[{"type":gate, "status":"True"}]}});
    let rejected = pods.patch_status(name, &PatchParams::default(), &Patch::Merge(&conditional)).await;
    anyhow::ensure!(matches!(rejected, Err(kube::Error::Api(ref response)) if response.code == 409),
        "status PATCH must still reject an explicit stale resourceVersion");
    context
        .wait_until("readiness-gated Pod Ready remains False", Duration::from_secs(30), || {
            let pods = pods.clone();
            async move {
                Ok(condition_status(&pods.get(name).await?, "Ready").as_deref() == Some("False"))
            }
        })
        .await?;

    let patch = json!({"status": {"conditions": [{"type": gate, "status": "True"}]}});
    pods.patch_status(name, &PatchParams::default(), &Patch::Merge(&patch))
        .await
        .context("satisfying readiness gate condition")?;
    context
        .wait_until("readiness-gated Pod Ready=True", Duration::from_secs(90), || {
            let pods = pods.clone();
            async move {
                Ok(condition_status(&pods.get(name).await?, "Ready").as_deref() == Some("True"))
            }
        })
        .await?;
    anyhow::ensure!(
        condition_status(&pods.get(name).await?, gate).as_deref() == Some("True"),
        "the external readiness-gate condition was lost during nodelet status updates"
    );
    let _ = pods.delete(name, &DeleteParams::default()).await;
    Ok(())
}
