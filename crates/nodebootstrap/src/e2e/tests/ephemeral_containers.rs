use super::context::E2eContext;
use anyhow::{Context, Result};
use http::Request;
use k8s_openapi::api::core::v1::Pod;
use kube::api::{Api, DeleteParams, PostParams};
use serde_json::json;
use std::time::Duration;

pub(super) async fn kubectl_debug_adds_and_starts_an_ephemeral_container(
    context: &E2eContext,
) -> Result<()> {
    let name = "ephemeral-check";
    let pods: Api<Pod> = Api::namespaced(context.client.clone(), &context.namespace);
    let pod: Pod = serde_json::from_value(json!({
        "apiVersion": "v1",
        "kind": "Pod",
        "metadata": {"name": name},
        "spec": {"containers": [{"name": "app", "image": "busybox:latest", "command": ["sleep", "3600"]}]}
    }))?;
    pods.create(&PostParams::default(), &pod)
        .await
        .context("creating ephemeral-container test Pod")?;
    context
        .wait_until("ephemeral-container test Pod Running", Duration::from_secs(90), || {
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

    let body = serde_json::to_vec(&json!({
        "spec": {"ephemeralContainers": [{
            "name": "debugger",
            "image": "busybox:latest",
            "command": ["sleep", "3600"],
            "targetContainerName": "app"
        }]}
    }))?;
    let request = Request::builder()
        .method("PATCH")
        .uri(format!(
            "/api/v1/namespaces/{}/pods/{}/ephemeralcontainers",
            context.namespace, name
        ))
        .header("Content-Type", "application/strategic-merge-patch+json")
        .body(body)
        .context("building ephemeralcontainers subresource patch")?;
    let _: Pod = context
        .client
        .request(request)
        .await
        .context("patching the ephemeralcontainers subresource")?;

    context
        .wait_until("ephemeral container status is Running", Duration::from_secs(90), || {
            let pods = pods.clone();
            async move {
                Ok(pods
                    .get(name)
                    .await?
                    .status
                    .and_then(|status| status.ephemeral_container_statuses)
                    .unwrap_or_default()
                    .into_iter()
                    .any(|status| {
                        status.name == "debugger"
                            && status
                                .state
                                .and_then(|state| state.running)
                                .is_some()
                    }))
            }
        })
        .await?;
    anyhow::ensure!(
        pods.get(name)
            .await?
            .status
            .and_then(|status| status.phase)
            .as_deref()
            == Some("Running"),
        "the Pod phase must remain Running after adding an ephemeral container"
    );
    let _ = pods.delete(name, &DeleteParams::default()).await;
    Ok(())
}
