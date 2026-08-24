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

pub(super) async fn restart_policy_never_exit_zero_is_succeeded(
    context: &E2eContext,
) -> Result<()> {
    let name = "restart-never-success";
    create_pod(
        context,
        name,
        json!({"restartPolicy": "Never", "containers": [{"name": "app", "image": "busybox:latest", "command": ["sh", "-c", "exit 0"]}]}),
    )
    .await?;
    context
        .wait_until("successful Never Pod", Duration::from_secs(90), || {
            phase_is(context, name, "Succeeded")
        })
        .await
}

pub(super) async fn restart_policy_never_nonzero_exit_is_failed(
    context: &E2eContext,
) -> Result<()> {
    let name = "restart-never-failure";
    create_pod(
        context,
        name,
        json!({"restartPolicy": "Never", "containers": [{"name": "app", "image": "busybox:latest", "command": ["sh", "-c", "exit 7"]}]}),
    )
    .await?;
    context
        .wait_until("failed Never Pod", Duration::from_secs(90), || {
            phase_is(context, name, "Failed")
        })
        .await
}

pub(super) async fn terminated_container_reports_its_exit_code(
    context: &E2eContext,
) -> Result<()> {
    let name = "terminated-exit-code";
    create_pod(
        context,
        name,
        json!({"restartPolicy": "Never", "containers": [{"name": "app", "image": "busybox:latest", "command": ["sh", "-c", "exit 7"]}]}),
    )
    .await?;
    let pods: Api<Pod> = Api::namespaced(context.client.clone(), &context.namespace);
    context
        .wait_until("terminated container exit code", Duration::from_secs(90), || {
            let pods = pods.clone();
            async move {
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
                    .is_some_and(|terminated| terminated.exit_code == 7))
            }
        })
        .await
}

pub(super) async fn guaranteed_pod_reports_guaranteed_qos(
    context: &E2eContext,
) -> Result<()> {
    let name = "guaranteed-qos";
    create_pod(
        context,
        name,
        json!({
            "containers": [{
                "name": "app",
                "image": "busybox:latest",
                "command": ["sleep", "30"],
                "resources": {"requests": {"cpu": "10m", "memory": "16Mi"}, "limits": {"cpu": "10m", "memory": "16Mi"}}
            }]
        }),
    )
    .await?;
    let pods: Api<Pod> = Api::namespaced(context.client.clone(), &context.namespace);
    context
        .wait_until("Guaranteed Pod qosClass", Duration::from_secs(90), || {
            let pods = pods.clone();
            async move {
                Ok(pods
                    .get(name)
                    .await?
                    .status
                    .and_then(|status| status.qos_class)
                    .as_deref()
                    == Some("Guaranteed"))
            }
        })
        .await
}

pub(super) async fn container_status_id_has_runtime_scheme(
    context: &E2eContext,
) -> Result<()> {
    let name = "container-id-scheme";
    create_pod(
        context,
        name,
        json!({"containers": [{"name": "app", "image": "busybox:latest", "command": ["sleep", "30"]}]}),
    )
    .await?;
    let pods: Api<Pod> = Api::namespaced(context.client.clone(), &context.namespace);
    context
        .wait_until("container runtime ID", Duration::from_secs(90), || {
            let pods = pods.clone();
            async move {
                Ok(pods
                    .get(name)
                    .await?
                    .status
                    .and_then(|status| status.container_statuses)
                    .unwrap_or_default()
                    .into_iter()
                    .find(|status| status.name == "app")
                    .and_then(|status| status.container_id)
                    .is_some_and(|id| id.contains("://")))
            }
        })
        .await
}
