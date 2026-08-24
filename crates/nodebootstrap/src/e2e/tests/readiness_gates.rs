use super::context::E2eContext;
use anyhow::{Context, Result};
use k8s_openapi::api::core::v1::Pod;
use kube::api::{Api, DeleteParams, Patch, PatchParams, PostParams};
use serde_json::json;
use std::time::Duration;

fn condition_status(pod: &Pod, condition_type: &str) -> Option<&str> {
    pod.status
        .as_ref()
        .and_then(|status| status.conditions.as_ref())
        .into_iter()
        .flatten()
        .find(|condition| condition.type_ == condition_type)
        .map(|condition| condition.status.as_str())
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
                Ok(condition_status(&pods.get(name).await?, "ContainersReady") == Some("True"))
            }
        })
        .await?;
    anyhow::ensure!(
        condition_status(&pods.get(name).await?, "Ready") == Some("False"),
        "Ready must be False while the readiness gate is unset"
    );

    let patch = json!({"status": {"conditions": [{"type": gate, "status": "False"}]}});
    pods.patch_status(name, &PatchParams::default(), &Patch::Merge(&patch))
        .await
        .context("clearing readiness gate condition")?;
    context
        .wait_until("readiness-gated Pod Ready remains False", Duration::from_secs(30), || {
            let pods = pods.clone();
            async move {
                Ok(condition_status(&pods.get(name).await?, "Ready") == Some("False"))
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
                Ok(condition_status(&pods.get(name).await?, "Ready") == Some("True"))
            }
        })
        .await?;
    anyhow::ensure!(
        condition_status(&pods.get(name).await?, gate) == Some("True"),
        "the external readiness-gate condition was lost during nodelet status updates"
    );
    let _ = pods.delete(name, &DeleteParams::default()).await;
    Ok(())
}
