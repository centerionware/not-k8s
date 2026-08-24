use super::context::E2eContext;
use anyhow::{Context, Result};
use k8s_openapi::api::core::v1::Pod;
use kube::api::{Api, PostParams};
use serde_json::json;
use std::time::Duration;

pub(super) async fn pod_condition_reports_observed_generation(
    context: &E2eContext,
) -> Result<()> {
    let name = "observed-generation-check";
    let pods: Api<Pod> = Api::namespaced(context.client.clone(), &context.namespace);
    let pod: Pod = serde_json::from_value(json!({
        "apiVersion": "v1",
        "kind": "Pod",
        "metadata": {"name": name},
        "spec": {"containers": [{"name": "app", "image": "busybox:latest", "command": ["sleep", "3600"]}]}
    }))?;
    pods.create(&PostParams::default(), &pod).await?;
    context
        .wait_until("Pod Ready observedGeneration", Duration::from_secs(90), || {
            let pods = pods.clone();
            async move {
                let pod = pods.get(name).await?;
                let Some(generation) = pod.metadata.generation else {
                    return Ok(false);
                };
                Ok(pod
                    .status
                    .and_then(|status| status.conditions)
                    .unwrap_or_default()
                    .into_iter()
                    .find(|condition| condition.type_ == "Ready")
                    .and_then(|condition| condition.observed_generation)
                    == Some(generation))
            }
        })
        .await
        .with_context(|| format!("checking Ready.observedGeneration for Pod {name}"))
}
