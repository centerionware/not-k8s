use super::context::E2eContext;
use super::skip_test;
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

async fn terminal_phase(context: &E2eContext, name: &str) -> Result<bool> {
    let pods: Api<Pod> = Api::namespaced(context.client.clone(), &context.namespace);
    Ok(pods
        .get(name)
        .await?
        .status
        .and_then(|status| status.phase)
        .is_some_and(|phase| phase == "Succeeded" || phase == "Failed"))
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

async fn read_cgroup_value(
    context: &E2eContext,
    name: &str,
    cgroup_file: &str,
    resources: serde_json::Value,
) -> Result<String> {
    create_pod(
        context,
        name,
        json!({
            "restartPolicy": "Never",
            "containers": [{"name": "app", "image": "busybox:latest", "resources": resources, "command": ["sh", "-c", &format!("if [ -r {cgroup_file} ]; then cat {cgroup_file} > /dev/termination-log; else exit 42; fi")] }]
        }),
    )
    .await?;
    context
        .wait_until("resource cgroup probe to finish", Duration::from_secs(90), || {
            terminal_phase(context, name)
        })
        .await?;
    termination_message(context, name)
        .await?
        .ok_or_else(|| skip_test(format!("{cgroup_file} is unavailable on this node; cgroup-v2 resource checks do not apply")))
}

pub(super) async fn memory_limit_is_enforced_via_cgroup(context: &E2eContext) -> Result<()> {
    let value = read_cgroup_value(
        context,
        "memory-limit-cgroup",
        "/sys/fs/cgroup/memory.max",
        json!({"limits": {"memory": "67108864"}}),
    )
    .await?;
    anyhow::ensure!(
        value.trim() == "67108864",
        "memory.max was {:?}, expected the Pod's 64Mi limit",
        value.trim()
    );
    Ok(())
}

pub(super) async fn cpu_limit_is_enforced_via_cgroup(context: &E2eContext) -> Result<()> {
    let value = read_cgroup_value(
        context,
        "cpu-limit-cgroup",
        "/sys/fs/cgroup/cpu.max",
        json!({"limits": {"cpu": "250m"}}),
    )
    .await?;
    anyhow::ensure!(
        value.trim() == "25000 100000",
        "cpu.max was {:?}, expected the Pod's 250m quota",
        value.trim()
    );
    Ok(())
}

pub(super) async fn besteffort_pod_gets_no_cgroup_limit(context: &E2eContext) -> Result<()> {
    let value = read_cgroup_value(
        context,
        "besteffort-cgroup",
        "/sys/fs/cgroup/memory.max",
        json!({}),
    )
    .await?;
    anyhow::ensure!(
        value.trim() == "max",
        "BestEffort memory.max was {:?}, expected max",
        value.trim()
    );
    Ok(())
}
