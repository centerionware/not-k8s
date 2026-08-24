use super::context::E2eContext;
use anyhow::Result;
use k8s_openapi::api::core::v1::Pod;
use kube::api::{Api, PostParams};
use serde_json::json;
use std::time::Duration;

async fn create_pod(context: &E2eContext, name: &str, spec: serde_json::Value) -> Result<()> {
    let pods: Api<Pod> = Api::namespaced(context.client.clone(), &context.namespace);
    let pod: Pod = serde_json::from_value(json!({
        "apiVersion": "v1",
        "kind": "Pod",
        "metadata": {"name": name},
        "spec": spec
    }))?;
    pods.create(&PostParams::default(), &pod).await?;
    Ok(())
}

async fn phase_is(context: &E2eContext, name: &str, expected: &str) -> Result<bool> {
    let pods: Api<Pod> = Api::namespaced(context.client.clone(), &context.namespace);
    Ok(pods
        .get(name)
        .await?
        .status
        .and_then(|status| status.phase)
        .as_deref()
        == Some(expected))
}

async fn termination_message(context: &E2eContext, name: &str) -> Result<Option<String>> {
    let pods: Api<Pod> = Api::namespaced(context.client.clone(), &context.namespace);
    Ok(pods
        .get(name)
        .await?
        .status
        .and_then(|status| status.container_statuses)
        .unwrap_or_default()
        .into_iter()
        .find(|status| status.name == "app")
        .and_then(|status| status.state)
        .and_then(|state| state.terminated)
        .and_then(|terminated| terminated.message))
}

pub(super) async fn read_only_root_filesystem_blocks_writes(
    context: &E2eContext,
) -> Result<()> {
    let name = "readonly-root-filesystem";
    create_pod(
        context,
        name,
        json!({
            "restartPolicy": "Never",
            "containers": [{
                "name": "app",
                "image": "busybox:latest",
                "command": ["sh", "-c", "touch /must-not-exist"],
                "securityContext": {"readOnlyRootFilesystem": true}
            }]
        }),
    )
    .await?;
    context
        .wait_until("read-only root Pod to fail its write", Duration::from_secs(90), || {
            phase_is(context, name, "Failed")
        })
        .await
}

pub(super) async fn writable_root_filesystem_allows_writes(context: &E2eContext) -> Result<()> {
    let name = "writable-root-filesystem";
    create_pod(
        context,
        name,
        json!({
            "restartPolicy": "Never",
            "containers": [{
                "name": "app",
                "image": "busybox:latest",
                "command": ["sh", "-c", "touch /write-is-allowed"]
            }]
        }),
    )
    .await?;
    context
        .wait_until("writable root Pod to complete", Duration::from_secs(90), || {
            phase_is(context, name, "Succeeded")
        })
        .await
}

pub(super) async fn run_as_user_is_applied(context: &E2eContext) -> Result<()> {
    let name = "run-as-user";
    create_pod(
        context,
        name,
        json!({
            "restartPolicy": "Never",
            "containers": [{"name": "app", "image": "busybox:latest", "command": ["sh", "-c", "id -u > /dev/termination-log"], "securityContext": {"runAsUser": 1000, "runAsGroup": 1000}}]
        }),
    )
    .await?;
    context
        .wait_until("runAsUser termination message", Duration::from_secs(90), || {
            let context = context.clone();
            async move {
                Ok(termination_message(&context, name)
                    .await?
                    .is_some_and(|message| message.trim() == "1000"))
            }
        })
        .await
}
