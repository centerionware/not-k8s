use super::context::E2eContext;
use anyhow::Result;
use k8s_openapi::api::core::v1::Pod;
use kube::api::{Api, DeleteParams, Patch, PatchParams, PostParams};
use serde_json::json;
use std::path::PathBuf;
use std::time::{Duration, Instant};

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

async fn release_deletion_finalizer(pods: &Api<Pod>, name: &str) -> Result<()> {
    pods.patch(
        name,
        &PatchParams::default(),
        &Patch::Merge(&json!({"metadata": {"finalizers": []}})),
    )
    .await?;
    Ok(())
}

async fn add_deletion_finalizer(pods: &Api<Pod>, name: &str) -> Result<()> {
    pods.patch(
        name,
        &PatchParams::default(),
        &Patch::Merge(&json!({"metadata": {"finalizers": ["nodebootstrap.e2e/observe-termination"]}})),
    )
    .await?;
    Ok(())
}

pub(super) async fn poststart_hook_runs_before_container_exit(
    context: &E2eContext,
) -> Result<()> {
    let name = "poststart-hook";
    create_pod(
        context,
        name,
        json!({
            "restartPolicy": "Never",
            "containers": [{
                "name": "app",
                "image": "busybox:latest",
                "command": ["sleep", "1"],
                "lifecycle": {"postStart": {"exec": {"command": ["sh", "-c", "echo poststart > /dev/termination-log"]}}}
            }]
        }),
    )
    .await?;
    context
        .wait_until("postStart termination message", Duration::from_secs(90), || {
            let context = context.clone();
            async move {
                Ok(termination_message(&context, name)
                    .await?
                    .is_some_and(|message| message.trim() == "poststart"))
            }
        })
        .await
}

pub(super) async fn termination_message_path_is_read_back_into_status(
    context: &E2eContext,
) -> Result<()> {
    let name = "custom-termination-message";
    create_pod(
        context,
        name,
        json!({
            "restartPolicy": "Never",
            "containers": [{
                "name": "app",
                "image": "busybox:latest",
                "command": ["sh", "-c", "echo custom-message > /tmp/message; exit 1"],
                "terminationMessagePath": "/tmp/message"
            }]
        }),
    )
    .await?;
    context
        .wait_until("custom termination message", Duration::from_secs(90), || {
            let context = context.clone();
            async move {
                Ok(termination_message(&context, name)
                    .await?
                    .is_some_and(|message| message.trim() == "custom-message"))
            }
        })
        .await
}

pub(super) async fn lifecycle_stop_signal_is_honored_by_the_runtime(
    context: &E2eContext,
) -> Result<()> {
    let name = "stop-signal-check";
    let pods: Api<Pod> = Api::namespaced(context.client.clone(), &context.namespace);
    create_pod(
        context,
        name,
        json!({
            "volumes": [{"name": "shared", "emptyDir": {}}],
            "containers": [{
                "name": "app",
                "image": "busybox:latest",
                // Keep the shell returning to its trap handler.  BusyBox ash
                // can defer traps while it waits for one long-lived child
                // (the former `sleep 3600`), which made this test report a
                // false failure even though CRI had sent SIGUSR1.
                "command": ["sh", "-c", "trap 'echo got-usr1 > /shared/signal.txt; exit 7' USR1; while true; do sleep 1; done"],
                "lifecycle": {"stopSignal": "SIGUSR1"},
                "volumeMounts": [{"name": "shared", "mountPath": "/shared"}]
            }],
            "os": {"name": "linux"}
        }),
    )
    .await?;
    context
        .wait_until("stop-signal Pod Running", Duration::from_secs(90), || {
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
    let pod_uid = pods
        .get(name)
        .await?
        .metadata
        .uid
        .ok_or_else(|| anyhow::anyhow!("stop-signal Pod has no UID"))?;
    let marker = PathBuf::from("/var/lib/nodelet/pods")
        .join(pod_uid)
        .join("volumes/shared/signal.txt");
    pods.delete(name, &DeleteParams::default()).await?;
    let result = context
        .wait_until(
            "configured stop signal termination message",
            Duration::from_secs(90),
            || {
                let marker = marker.clone();
                async move {
                    Ok(std::fs::read_to_string(&marker)
                        .is_ok_and(|message| message.trim() == "got-usr1"))
                }
            },
        )
        .await;
    let _ = std::fs::remove_file(marker);
    result
}

pub(super) async fn prestop_hook_runs_before_termination(context: &E2eContext) -> Result<()> {
    let name = "prestop-hook";
    let pods: Api<Pod> = Api::namespaced(context.client.clone(), &context.namespace);
    create_pod(
        context,
        name,
        json!({
            "terminationGracePeriodSeconds": 15,
            "volumes": [{"name": "shared", "emptyDir": {}}],
            "containers": [{
                "name": "app",
                "image": "busybox:latest",
                "command": ["sh", "-c", "trap 'while true; do sleep 1; done' TERM; while true; do sleep 1; done"],
                "lifecycle": {"preStop": {"exec": {"command": ["sh", "-c", "echo prestop > /shared/prestop.txt"]}}},
                "volumeMounts": [{"name": "shared", "mountPath": "/shared"}]
            }]
        }),
    )
    .await?;
    context
        .wait_until("preStop Pod Running", Duration::from_secs(90), || {
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
    let pod_uid = pods
        .get(name)
        .await?
        .metadata
        .uid
        .ok_or_else(|| anyhow::anyhow!("preStop Pod has no UID"))?;
    let marker = PathBuf::from("/var/lib/nodelet/pods")
        .join(pod_uid)
        .join("volumes/shared/prestop.txt");
    add_deletion_finalizer(&pods, name).await?;
    pods.delete(name, &DeleteParams::default()).await?;
    let result = context
        .wait_until("preStop marker", Duration::from_secs(30), || {
            let marker = marker.clone();
            async move {
                Ok(std::fs::read_to_string(&marker)
                    .is_ok_and(|message| message.trim() == "prestop"))
            }
        })
        .await;
    release_deletion_finalizer(&pods, name).await?;
    let _ = std::fs::remove_file(marker);
    result
}

pub(super) async fn termination_grace_period_is_honored_not_instant(
    context: &E2eContext,
) -> Result<()> {
    let name = "grace-period";
    let pods: Api<Pod> = Api::namespaced(context.client.clone(), &context.namespace);
    create_pod(
        context,
        name,
        json!({
            "terminationGracePeriodSeconds": 8,
            "containers": [{
                "name": "app",
                "image": "busybox:latest",
                "command": ["sh", "-c", "trap 'echo trapped' TERM; while true; do sleep 1; done"]
            }]
        }),
    )
    .await?;
    context
        .wait_until("grace-period Pod Running", Duration::from_secs(90), || {
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
    let started = Instant::now();
    pods.delete(name, &DeleteParams::default()).await?;
    let deleting = pods
        .get(name)
        .await
        .context("reading the Pod after its graceful delete request")?;
    anyhow::ensure!(
        deleting.metadata.deletion_timestamp.is_some()
            && deleting.metadata.deletion_grace_period_seconds == Some(8),
        "graceful Pod delete did not persist deletion metadata: {:?}",
        deleting.metadata
    );
    context
        .wait_until("grace-period Pod deletion", Duration::from_secs(60), || {
            let pods = pods.clone();
            async move { Ok(pods.get_opt(name).await?.is_none()) }
        })
        .await?;
    anyhow::ensure!(
        started.elapsed() >= Duration::from_secs(5),
        "Pod disappeared after {:?}; terminationGracePeriodSeconds was not honored",
        started.elapsed()
    );
    Ok(())
}
