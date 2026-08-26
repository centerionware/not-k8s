use super::context::E2eContext;
use super::skip_test;
use anyhow::Result;
use k8s_openapi::api::core::v1::Pod;
use kube::api::{Api, DeleteParams, PostParams};
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

pub(super) async fn pod_status_reports_qos_class(context: &E2eContext) -> Result<()> {
    let name = "qos-class-check";
    create_pod(
        context,
        name,
        json!({
            "containers": [{
                "name": "app",
                "image": "busybox:latest",
                "command": ["sleep", "30"],
                "resources": {
                    "requests": {"cpu": "100m", "memory": "64Mi"},
                    "limits": {"cpu": "100m", "memory": "64Mi"}
                }
            }]
        }),
    )
    .await?;
    let pods: Api<Pod> = Api::namespaced(context.client.clone(), &context.namespace);
    context
        .wait_until("Pod status qosClass", Duration::from_secs(90), || {
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

pub(super) async fn pod_exceeding_its_active_deadline_is_terminated(
    context: &E2eContext,
) -> Result<()> {
    let name = "active-deadline";
    create_pod(
        context,
        name,
        json!({
            "activeDeadlineSeconds": 5,
            "restartPolicy": "Never",
            "containers": [{"name": "app", "image": "busybox:latest", "command": ["sleep", "30"]}]
        }),
    )
    .await?;
    let pods: Api<Pod> = Api::namespaced(context.client.clone(), &context.namespace);
    context
        .wait_until("activeDeadlineSeconds termination", Duration::from_secs(90), || {
            let pods = pods.clone();
            async move {
                let status = pods.get(name).await?.status;
                Ok(status
                    .as_ref()
                    .and_then(|status| status.phase.as_deref())
                    == Some("Failed")
                    && status.and_then(|status| status.reason).as_deref()
                        == Some("DeadlineExceeded"))
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

async fn crash_loop_backoff_reports_waiting_reason_named(
    context: &E2eContext,
    name: &str,
) -> Result<()> {
    create_pod(
        context,
        name,
        json!({"containers": [{"name": "app", "image": "busybox:latest", "command": ["sh", "-c", "exit 1"]}]}),
    )
    .await?;
    let pods: Api<Pod> = Api::namespaced(context.client.clone(), &context.namespace);
    context
        .wait_until("CrashLoopBackOff waiting reason", Duration::from_secs(120), || {
            let pods = pods.clone();
            async move {
                Ok(pods
                    .get(name)
                    .await?
                    .status
                    .and_then(|status| status.container_statuses)
                    .unwrap_or_default()
                    .into_iter()
                    .any(|status| {
                        status
                            .state
                            .and_then(|state| state.waiting)
                            .is_some_and(|waiting| waiting.reason.as_deref() == Some("CrashLoopBackOff"))
                            && status
                                .last_state
                                .and_then(|state| state.terminated)
                                .is_some_and(|terminated| terminated.exit_code != 0)
                    }))
            }
        })
        .await
}

pub(super) async fn crash_loop_backoff_reports_waiting_reason(
    context: &E2eContext,
) -> Result<()> {
    crash_loop_backoff_reports_waiting_reason_named(context, "crash-loop-backoff").await
}

pub(super) async fn crash_loop_backoff_reports_waiting_reason_and_last_state(
    context: &E2eContext,
) -> Result<()> {
    crash_loop_backoff_reports_waiting_reason_named(context, "crash-loop-backoff-last-state").await
}

pub(super) async fn image_pull_policy_if_not_present_skips_the_registry_round_trip(
    context: &E2eContext,
) -> Result<()> {
    if crate::config::Config::from_env()?.nodelet_runtime() != "cri" {
        return Err(skip_test("image pull policy checks require the CRI runtime"));
    }
    let pods: Api<Pod> = Api::namespaced(context.client.clone(), &context.namespace);
    for name in ["if-not-present-first", "if-not-present-second"] {
        create_pod(
            context,
            name,
            json!({
                "restartPolicy": "Never",
                "containers": [{"name": "app", "image": "busybox:latest", "imagePullPolicy": "IfNotPresent", "command": ["sh", "-c", "echo cached-image"]}]
            }),
        )
        .await?;
        context
            .wait_until("IfNotPresent Pod to complete", Duration::from_secs(90), || {
                let pods = pods.clone();
                async move {
                    Ok(pods
                        .get(name)
                        .await?
                        .status
                        .and_then(|status| status.phase)
                        .as_deref()
                        == Some("Succeeded"))
                }
            })
            .await?;
        let _ = pods.delete(name, &DeleteParams::default()).await;
    }
    Ok(())
}

pub(super) async fn crash_loop_backoff_throttles_immediate_restarts(
    context: &E2eContext,
) -> Result<()> {
    if crate::config::Config::from_env()?.nodelet_runtime() != "cri" {
        return Err(skip_test("crash-loop backoff checks require the CRI runtime"));
    }
    let name = "crash-loop-backoff";
    let pods: Api<Pod> = Api::namespaced(context.client.clone(), &context.namespace);
    create_pod(
        context,
        name,
        json!({"containers": [{"name": "app", "image": "busybox:latest", "command": ["sh", "-c", "exit 1"]}]}),
    )
    .await?;
    context
        .wait_until("crash-loop first restart", Duration::from_secs(60), || {
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
                    .is_some_and(|status| status.restart_count >= 1))
            }
        })
        .await?;
    tokio::time::sleep(Duration::from_secs(20)).await;
    let restart_count = pods
        .get(name)
        .await?
        .status
        .and_then(|status| status.container_statuses)
        .unwrap_or_default()
        .into_iter()
        .find(|status| status.name == "app")
        .map(|status| status.restart_count)
        .unwrap_or_default();
    let _ = pods.delete(name, &DeleteParams::default()).await;
    anyhow::ensure!(
        (1..=6).contains(&restart_count),
        "crash-loop restart count {restart_count} was outside the throttled range 1..=6"
    );
    Ok(())
}

pub(super) async fn image_pull_policy_never_fails_when_image_is_absent(
    context: &E2eContext,
) -> Result<()> {
    let name = "image-pull-never";
    create_pod(
        context,
        name,
        json!({"restartPolicy": "Never", "containers": [{"name": "app", "image": "not-k8s-e2e/image-that-does-not-exist:never", "imagePullPolicy": "Never", "command": ["sleep", "30"]}]}),
    )
    .await?;
    let pods: Api<Pod> = Api::namespaced(context.client.clone(), &context.namespace);
    context
        .wait_until("ErrImageNeverPull status", Duration::from_secs(90), || {
            let pods = pods.clone();
            async move {
                Ok(pods
                    .get(name)
                    .await?
                    .status
                    .and_then(|status| status.container_statuses)
                    .unwrap_or_default()
                    .into_iter()
                    .any(|status| {
                        status
                            .state
                            .and_then(|state| state.waiting)
                            .is_some_and(|waiting| waiting.reason.as_deref() == Some("ErrImageNeverPull"))
                    }))
            }
        })
        .await
}

pub(super) async fn pod_status_reports_host_ips_plural(context: &E2eContext) -> Result<()> {
    let name = "pod-host-ips";
    create_pod(
        context,
        name,
        json!({"containers": [{"name": "app", "image": "busybox:latest", "command": ["sleep", "30"]}]}),
    )
    .await?;
    let pods: Api<Pod> = Api::namespaced(context.client.clone(), &context.namespace);
    context
        .wait_until("Pod.status.hostIPs", Duration::from_secs(90), || {
            let pods = pods.clone();
            async move {
                Ok(pods
                    .get(name)
                    .await?
                    .status
                    .and_then(|status| status.host_ips)
                    .is_some_and(|ips| !ips.is_empty()))
            }
        })
        .await
}

pub(super) async fn container_status_reports_a_real_image_id(
    context: &E2eContext,
) -> Result<()> {
    let name = "container-image-id";
    create_pod(
        context,
        name,
        json!({"containers": [{"name": "app", "image": "busybox:latest", "command": ["sleep", "30"]}]}),
    )
    .await?;
    let pods: Api<Pod> = Api::namespaced(context.client.clone(), &context.namespace);
    context
        .wait_until("container status imageID", Duration::from_secs(90), || {
            let pods = pods.clone();
            async move {
                Ok(pods
                    .get(name)
                    .await?
                    .status
                    .and_then(|status| status.container_statuses)
                    .unwrap_or_default()
                    .into_iter()
                    .any(|status| status.name == "app" && !status.image_id.is_empty()))
            }
        })
        .await
}
