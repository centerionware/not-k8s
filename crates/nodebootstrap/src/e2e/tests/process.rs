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

async fn pod_reached_running(context: &E2eContext, name: &str) -> Result<bool> {
    let pods: Api<Pod> = Api::namespaced(context.client.clone(), &context.namespace);
    Ok(pods
        .get(name)
        .await?
        .status
        .and_then(|status| status.phase)
        .as_deref()
        == Some("Running"))
}

pub(super) async fn containers_get_isolated_pid_namespaces_by_default(
    context: &E2eContext,
) -> Result<()> {
    let name = "pid-isolated-default";
    create_pod(
        context,
        name,
        json!({
            "restartPolicy": "Never",
            "volumes": [{"name": "shared", "emptyDir": {}}],
            "containers": [
                {"name": "app", "image": "busybox:latest", "command": ["sh", "-c", "while [ ! -f /shared/second-pid ]; do sleep 1; done; cat /shared/second-pid > /dev/termination-log"], "volumeMounts": [{"name": "shared", "mountPath": "/shared"}]},
                {"name": "second", "image": "busybox:latest", "command": ["sh", "-c", "echo $$$$ > /shared/second-pid; sleep 30"], "volumeMounts": [{"name": "shared", "mountPath": "/shared"}]}
            ]
        }),
    )
    .await?;
    context
        .wait_until("isolated PID namespace Pod", Duration::from_secs(90), || {
            pod_reached_running(context, name)
        })
        .await?;
    context
        .wait_until("isolated second-container PID", Duration::from_secs(30), || {
            let context = context.clone();
            async move {
                Ok(termination_message(&context, name)
                    .await?
                    .is_some_and(|message| message.trim() == "1"))
            }
        })
        .await
}

pub(super) async fn share_process_namespace_puts_every_container_in_one_pid_namespace(
    context: &E2eContext,
) -> Result<()> {
    let name = "pid-shared";
    create_pod(
        context,
        name,
        json!({
            "restartPolicy": "Never",
            "shareProcessNamespace": true,
            "volumes": [{"name": "shared", "emptyDir": {}}],
            "containers": [
                {"name": "app", "image": "busybox:latest", "command": ["sh", "-c", "while [ ! -f /shared/second-pid ]; do sleep 1; done; cat /shared/second-pid > /dev/termination-log"], "volumeMounts": [{"name": "shared", "mountPath": "/shared"}]},
                {"name": "second", "image": "busybox:latest", "command": ["sh", "-c", "echo $$$$ > /shared/second-pid; sleep 30"], "volumeMounts": [{"name": "shared", "mountPath": "/shared"}]}
            ]
        }),
    )
    .await?;
    context
        .wait_until("shared PID namespace Pod", Duration::from_secs(90), || {
            pod_reached_running(context, name)
        })
        .await?;
    context
        .wait_until("shared second-container PID", Duration::from_secs(30), || {
            let context = context.clone();
            async move {
                Ok(termination_message(&context, name)
                    .await?
                    .and_then(|message| message.trim().parse::<u32>().ok())
                    .is_some_and(|pid| pid != 1))
            }
        })
        .await
}

pub(super) async fn host_pid_sees_host_processes(context: &E2eContext) -> Result<()> {
    let name = "host-pid-check";
    create_pod(
        context,
        name,
        json!({
            "restartPolicy": "Never",
            "hostPID": true,
            "containers": [{"name": "app", "image": "busybox:latest", "command": ["sh", "-c", "ls /proc | grep -E '^[0-9]+$' | wc -l > /dev/termination-log"]}]
        }),
    )
    .await?;
    context
        .wait_until("host PID process count", Duration::from_secs(90), || {
            let context = context.clone();
            async move {
                Ok(termination_message(&context, name)
                    .await?
                    .and_then(|message| message.trim().parse::<u32>().ok())
                    .is_some_and(|count| count > 5))
            }
        })
        .await
}
