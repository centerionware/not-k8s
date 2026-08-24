use super::context::E2eContext;
use anyhow::{Context, Result};
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
    pods.create(&PostParams::default(), &pod)
        .await
        .with_context(|| format!("creating Pod {name}"))?;
    Ok(())
}

async fn pod_has_phase(context: &E2eContext, name: &str, phase: &str) -> Result<bool> {
    let pods: Api<Pod> = Api::namespaced(context.client.clone(), &context.namespace);
    Ok(pods
        .get(name)
        .await?
        .status
        .and_then(|status| status.phase)
        .as_deref()
        == Some(phase))
}

pub(super) async fn basic_pod_runs(context: &E2eContext) -> Result<()> {
    let name = "basic-pod";
    create_pod(
        context,
        name,
        json!({"containers": [{"name": "app", "image": "busybox:latest", "command": ["sleep", "3600"]}]}),
    )
    .await?;
    context
        .wait_until("basic Pod Running", Duration::from_secs(90), || async {
            pod_has_phase(context, name, "Running").await
        })
        .await?;
    let pods: Api<Pod> = Api::namespaced(context.client.clone(), &context.namespace);
    let pod = pods.get(name).await?;
    let ready = pod
        .status
        .and_then(|status| status.conditions)
        .unwrap_or_default()
        .iter()
        .any(|condition| condition.type_ == "Ready" && condition.status == "True");
    anyhow::ensure!(ready, "basic Pod never reported Ready=True");
    Ok(())
}

pub(super) async fn init_containers_run_before_app_container(
    context: &E2eContext,
) -> Result<()> {
    let name = "init-order";
    create_pod(
        context,
        name,
        json!({
            "initContainers": [
                {"name": "init-one", "image": "busybox:latest", "command": ["sh", "-c", "sleep 2"]},
                {"name": "init-two", "image": "busybox:latest", "command": ["sh", "-c", "sleep 2"]}
            ],
            "containers": [{"name": "app", "image": "busybox:latest", "command": ["sleep", "3600"]}]
        }),
    )
    .await?;
    context
        .wait_until(
            "init-order Pod Running",
            Duration::from_secs(120),
            || async { pod_has_phase(context, name, "Running").await },
        )
        .await?;
    let pods: Api<Pod> = Api::namespaced(context.client.clone(), &context.namespace);
    let pod = pods.get(name).await?;
    let initialized = pod
        .status
        .as_ref()
        .and_then(|status| status.conditions.as_ref())
        .into_iter()
        .flatten()
        .any(|condition| condition.type_ == "Initialized" && condition.status == "True");
    anyhow::ensure!(
        initialized,
        "Pod ran its app container without reporting Initialized=True"
    );
    let names: Vec<_> = pod
        .status
        .and_then(|status| status.init_container_statuses)
        .unwrap_or_default()
        .into_iter()
        .map(|status| status.name)
        .collect();
    anyhow::ensure!(
        names == vec!["init-one".to_string(), "init-two".to_string()],
        "initContainerStatuses order was {names:?}"
    );
    Ok(())
}

pub(super) async fn native_sidecar_starts_before_app_container(
    context: &E2eContext,
) -> Result<()> {
    let name = "native-sidecar";
    create_pod(
        context,
        name,
        json!({
            "initContainers": [{"name": "proxy", "image": "busybox:latest", "restartPolicy": "Always", "command": ["sleep", "3600"]}],
            "containers": [{"name": "app", "image": "busybox:latest", "command": ["sleep", "3600"]}]
        }),
    )
    .await?;
    context
        .wait_until(
            "native-sidecar Pod Running",
            Duration::from_secs(90),
            || async { pod_has_phase(context, name, "Running").await },
        )
        .await?;
    let pods: Api<Pod> = Api::namespaced(context.client.clone(), &context.namespace);
    let pod = pods.get(name).await?;
    let initialized = pod
        .status
        .as_ref()
        .and_then(|status| status.conditions.as_ref())
        .into_iter()
        .flatten()
        .any(|condition| condition.type_ == "Initialized" && condition.status == "True");
    let sidecar_running = pod
        .status
        .and_then(|status| status.init_container_statuses)
        .unwrap_or_default()
        .first()
        .and_then(|status| status.state.as_ref())
        .and_then(|state| state.running.as_ref())
        .is_some();
    anyhow::ensure!(
        initialized && sidecar_running,
        "native sidecar did not remain running while the app Pod started"
    );
    Ok(())
}

pub(super) async fn native_sidecar_restarts_on_crash(context: &E2eContext) -> Result<()> {
    let name = "native-sidecar-crash";
    create_pod(
        context,
        name,
        json!({
            "initContainers": [{"name": "proxy", "image": "busybox:latest", "restartPolicy": "Always", "command": ["sh", "-c", "sleep 3; exit 1"]}],
            "containers": [{"name": "app", "image": "busybox:latest", "command": ["sleep", "3600"]}]
        }),
    )
    .await?;
    context
        .wait_until(
            "native sidecar Pod Running",
            Duration::from_secs(120),
            || async { pod_has_phase(context, name, "Running").await },
        )
        .await?;
    let pods: Api<Pod> = Api::namespaced(context.client.clone(), &context.namespace);
    context
        .wait_until("native sidecar restartCount > 0", Duration::from_secs(90), || {
            let pods = pods.clone();
            async move {
                Ok(pods
                    .get(name)
                    .await?
                    .status
                    .and_then(|status| status.init_container_statuses)
                    .unwrap_or_default()
                    .first()
                    .is_some_and(|status| status.restart_count > 0))
            }
        })
        .await
}

pub(super) async fn init_failure_blocks_app(context: &E2eContext) -> Result<()> {
    let name = "init-fail-never";
    create_pod(
        context,
        name,
        json!({
            "restartPolicy": "Never",
            "initContainers": [{"name": "doomed", "image": "busybox:latest", "command": ["sh", "-c", "exit 7"]}],
            "containers": [{"name": "app", "image": "busybox:latest", "command": ["sleep", "3600"]}]
        }),
    )
    .await?;
    context
        .wait_until("failed init Pod", Duration::from_secs(90), || async {
            pod_has_phase(context, name, "Failed").await
        })
        .await
}

pub(super) async fn crashing_container_restarts(context: &E2eContext) -> Result<()> {
    let name = "crash-loop";
    create_pod(
        context,
        name,
        json!({"containers": [{"name": "app", "image": "busybox:latest", "command": ["sh", "-c", "sleep 3; exit 1"]}]}),
    )
    .await?;
    let pods: Api<Pod> = Api::namespaced(context.client.clone(), &context.namespace);
    context
        .wait_until("crash-loop restartCount > 0", Duration::from_secs(120), || {
            let pods = pods.clone();
            async move {
                Ok(pods
                    .get(name)
                    .await?
                    .status
                    .and_then(|status| status.container_statuses)
                    .unwrap_or_default()
                    .first()
                    .is_some_and(|status| status.restart_count > 0))
            }
        })
        .await
}
