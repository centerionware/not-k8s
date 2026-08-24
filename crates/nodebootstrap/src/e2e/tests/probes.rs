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

async fn pod_ready(context: &E2eContext, name: &str) -> Result<bool> {
    let pods: Api<Pod> = Api::namespaced(context.client.clone(), &context.namespace);
    Ok(pods
        .get(name)
        .await?
        .status
        .and_then(|status| status.conditions)
        .unwrap_or_default()
        .into_iter()
        .any(|condition| condition.type_ == "Ready" && condition.status == "True"))
}

async fn pod_not_ready(context: &E2eContext, name: &str) -> Result<bool> {
    let pods: Api<Pod> = Api::namespaced(context.client.clone(), &context.namespace);
    Ok(pods
        .get(name)
        .await?
        .status
        .and_then(|status| status.conditions)
        .unwrap_or_default()
        .into_iter()
        .any(|condition| condition.type_ == "Ready" && condition.status == "False"))
}

pub(super) async fn readiness_probe_gates_ready_condition(context: &E2eContext) -> Result<()> {
    let name = "readiness-probe";
    create_pod(
        context,
        name,
        json!({
            "containers": [{
                "name": "app",
                "image": "busybox:latest",
                "command": ["sh", "-c", "while true; do nc -l -p 8080; done"],
                "readinessProbe": {"tcpSocket": {"port": 8080}, "periodSeconds": 1}
            }]
        }),
    )
    .await?;
    context
        .wait_until("readiness probe to report Ready", Duration::from_secs(90), || {
            pod_ready(context, name)
        })
        .await
}

pub(super) async fn liveness_probe_failure_restarts_container(
    context: &E2eContext,
) -> Result<()> {
    let name = "liveness-probe-failure";
    create_pod(
        context,
        name,
        json!({
            "containers": [{
                "name": "app",
                "image": "busybox:latest",
                "command": ["sleep", "3600"],
                "livenessProbe": {"exec": {"command": ["sh", "-c", "exit 1"]}, "initialDelaySeconds": 1, "periodSeconds": 2, "failureThreshold": 1}
            }]
        }),
    )
    .await?;
    let pods: Api<Pod> = Api::namespaced(context.client.clone(), &context.namespace);
    context
        .wait_until("liveness probe restartCount to increase", Duration::from_secs(90), || {
            let pods = pods.clone();
            async move {
                Ok(pods
                    .get(name)
                    .await?
                    .status
                    .and_then(|status| status.container_statuses)
                    .unwrap_or_default()
                    .into_iter()
                    .any(|status| status.restart_count > 0))
            }
        })
        .await
}

pub(super) async fn liveness_probe_uses_its_own_grace_period(
    context: &E2eContext,
) -> Result<()> {
    let name = "liveness-grace-override";
    create_pod(
        context,
        name,
        json!({
            "terminationGracePeriodSeconds": 60,
            "containers": [{
                "name": "app",
                "image": "busybox:latest",
                "command": ["sh", "-c", "trap '' TERM; touch /tmp/alive; sleep 6; rm -f /tmp/alive; sleep 3600"],
                "livenessProbe": {
                    "exec": {"command": ["test", "-f", "/tmp/alive"]},
                    "periodSeconds": 2,
                    "failureThreshold": 2,
                    "terminationGracePeriodSeconds": 3
                }
            }]
        }),
    )
    .await?;
    let pods: Api<Pod> = Api::namespaced(context.client.clone(), &context.namespace);
    context
        .wait_until("liveness grace-period restart", Duration::from_secs(60), || {
            let pods = pods.clone();
            async move {
                Ok(pods
                    .get(name)
                    .await?
                    .status
                    .and_then(|status| status.container_statuses)
                    .unwrap_or_default()
                    .into_iter()
                    .any(|status| status.restart_count > 0))
            }
        })
        .await
}

pub(super) async fn startup_probe_gates_readiness_until_server_starts(
    context: &E2eContext,
) -> Result<()> {
    let name = "startup-probe";
    create_pod(
        context,
        name,
        json!({
            "containers": [{
                "name": "app",
                "image": "busybox:latest",
                "command": ["sh", "-c", "sleep 5; while true; do nc -l -p 8080; done"],
                "startupProbe": {"tcpSocket": {"port": 8080}, "periodSeconds": 1, "failureThreshold": 10},
                "readinessProbe": {"tcpSocket": {"port": 8080}, "periodSeconds": 1}
            }]
        }),
    )
    .await?;
    context
        .wait_until("startup probe to allow readiness", Duration::from_secs(90), || {
            pod_ready(context, name)
        })
        .await
}

pub(super) async fn startup_probe_gates_liveness_and_readiness(
    context: &E2eContext,
) -> Result<()> {
    let name = "startup-gate";
    create_pod(
        context,
        name,
        json!({
            "containers": [{
                "name": "app",
                "image": "busybox:latest",
                "command": ["sh", "-c", "sleep 6; touch /tmp/started; sleep 3600"],
                "startupProbe": {
                    "exec": {"command": ["test", "-f", "/tmp/started"]},
                    "periodSeconds": 2,
                    "failureThreshold": 30
                },
                "livenessProbe": {
                    "exec": {"command": ["test", "-f", "/tmp/started"]},
                    "periodSeconds": 2
                },
                "readinessProbe": {
                    "exec": {"command": ["test", "-f", "/tmp/started"]},
                    "periodSeconds": 2
                }
            }]
        }),
    )
    .await?;
    let pods: Api<Pod> = Api::namespaced(context.client.clone(), &context.namespace);
    context
        .wait_until("startup-gate Pod Running", Duration::from_secs(90), || {
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
    tokio::time::sleep(Duration::from_secs(2)).await;
    anyhow::ensure!(
        pod_not_ready(context, name).await?,
        "readiness must be false while the startup probe is still failing"
    );
    context
        .wait_until("startup-gate Pod Ready", Duration::from_secs(90), || {
            pod_ready(context, name)
        })
        .await
}

pub(super) async fn startup_probe_failure_past_threshold_restarts_the_container(
    context: &E2eContext,
) -> Result<()> {
    let name = "startup-probe-failure";
    create_pod(
        context,
        name,
        json!({
            "containers": [{
                "name": "app",
                "image": "busybox:latest",
                "command": ["sleep", "3600"],
                "startupProbe": {"exec": {"command": ["sh", "-c", "exit 1"]}, "initialDelaySeconds": 1, "periodSeconds": 1, "failureThreshold": 1}
            }]
        }),
    )
    .await?;
    let pods: Api<Pod> = Api::namespaced(context.client.clone(), &context.namespace);
    context
        .wait_until("startup probe failure restartCount", Duration::from_secs(90), || {
            let pods = pods.clone();
            async move {
                Ok(pods
                    .get(name)
                    .await?
                    .status
                    .and_then(|status| status.container_statuses)
                    .unwrap_or_default()
                    .into_iter()
                    .any(|status| status.restart_count > 0))
            }
        })
        .await
}

pub(super) async fn http_get_readiness_probe_against_a_real_server(
    context: &E2eContext,
) -> Result<()> {
    let name = "http-readiness-probe";
    create_pod(
        context,
        name,
        json!({
            "containers": [{
                "name": "app",
                "image": "busybox:latest",
                "command": ["sh", "-c", "printf 'HTTP/1.1 200 OK\\r\\nContent-Length: 2\\r\\nConnection: close\\r\\n\\r\\nok' > /tmp/response; while true; do nc -l -p 8080 < /tmp/response; done"],
                "readinessProbe": {"httpGet": {"path": "/healthz", "port": 8080}, "periodSeconds": 1}
            }]
        }),
    )
    .await?;
    context
        .wait_until("HTTP readiness probe to report Ready", Duration::from_secs(90), || {
            pod_ready(context, name)
        })
        .await
}

pub(super) async fn wrong_port_readiness_probe_keeps_pod_not_ready(
    context: &E2eContext,
) -> Result<()> {
    let name = "readiness-wrong-port";
    create_pod(
        context,
        name,
        json!({
            "containers": [{
                "name": "app",
                "image": "busybox:latest",
                "command": ["sleep", "3600"],
                "readinessProbe": {"tcpSocket": {"port": 6553}, "periodSeconds": 1}
            }]
        }),
    )
    .await?;
    context
        .wait_until("wrong-port readiness probe to report False", Duration::from_secs(90), || {
            pod_not_ready(context, name)
        })
        .await
}
