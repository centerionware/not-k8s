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

pub(super) async fn env_resource_field_ref_reports_the_containers_own_limits(
    context: &E2eContext,
) -> Result<()> {
    let name = "resource-field-ref";
    create_pod(
        context,
        name,
        json!({
            "restartPolicy": "Never",
            "containers": [{
                "name": "app",
                "image": "busybox:latest",
                "resources": {"limits": {"cpu": "1500m", "memory": "536870912"}},
                "env": [
                    {"name": "CPU_LIMIT_CORES", "valueFrom": {"resourceFieldRef": {"resource": "limits.cpu"}}},
                    {"name": "MEM_LIMIT_MI", "valueFrom": {"resourceFieldRef": {"resource": "limits.memory", "divisor": "1Mi"}}}
                ],
                "command": ["sh", "-c", "echo \"$CPU_LIMIT_CORES:$MEM_LIMIT_MI\" > /dev/termination-log"]
            }]
        }),
    )
    .await?;
    context
        .wait_until("resourceFieldRef values", Duration::from_secs(90), || {
            let context = context.clone();
            async move {
                Ok(termination_message(&context, name)
                    .await?
                    .is_some_and(|message| message.trim() == "2:512"))
            }
        })
        .await
}

async fn oom_score(context: &E2eContext, name: &str, resources: serde_json::Value) -> Result<String> {
    create_pod(
        context,
        name,
        json!({
            "restartPolicy": "Never",
            "containers": [{
                "name": "app",
                "image": "busybox:latest",
                "resources": resources,
                "command": ["sh", "-c", "cat /proc/self/oom_score_adj > /dev/termination-log"]
            }]
        }),
    )
    .await?;
    context
        .wait_until("Pod OOM score probe", Duration::from_secs(90), || {
            let context = context.clone();
            async move { Ok(terminal_phase(&context, name).await? && termination_message(&context, name).await?.is_some()) }
        })
        .await?;
    termination_message(context, name)
        .await?
        .ok_or_else(|| anyhow::anyhow!("OOM score probe terminated without a message"))
}

pub(super) async fn besteffort_pod_gets_the_certain_death_oom_score(
    context: &E2eContext,
) -> Result<()> {
    let value = oom_score(context, "oom-score-besteffort", json!({})).await?;
    anyhow::ensure!(
        value.trim() == "1000",
        "BestEffort oom_score_adj was {:?}, expected 1000",
        value.trim()
    );
    Ok(())
}

pub(super) async fn guaranteed_pod_gets_the_protected_oom_score(
    context: &E2eContext,
) -> Result<()> {
    let value = oom_score(
        context,
        "oom-score-guaranteed",
        json!({"requests": {"cpu": "100m", "memory": "64Mi"}, "limits": {"cpu": "100m", "memory": "64Mi"}}),
    )
    .await?;
    anyhow::ensure!(
        value.trim() == "-998",
        "Guaranteed oom_score_adj was {:?}, expected -998",
        value.trim()
    );
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

pub(super) async fn no_swap_default_disables_swap_via_cgroup(
    context: &E2eContext,
) -> Result<()> {
    let value = read_cgroup_value(
        context,
        "no-swap-cgroup",
        "/sys/fs/cgroup/memory.swap.max",
        json!({"limits": {"memory": "67108864"}}),
    )
    .await?;
    anyhow::ensure!(
        value.trim() == "0",
        "default NoSwap memory.swap.max was {:?}, expected 0",
        value.trim()
    );
    Ok(())
}

pub(super) async fn hugepages_limit_is_enforced_via_cgroup(
    context: &E2eContext,
) -> Result<()> {
    if std::fs::read_to_string("/proc/sys/vm/nr_hugepages")
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .unwrap_or_default()
        == 0
    {
        return Err(skip_test(
            "no hugepages are reserved on this node; HugePages cgroup limits cannot be exercised",
        ));
    }
    let value = read_cgroup_value(
        context,
        "hugepages-limit-cgroup",
        "/sys/fs/cgroup/hugetlb.2MB.max",
        json!({"limits": {"hugepages-2Mi": "4Mi", "memory": "67108864"}}),
    )
    .await?;
    anyhow::ensure!(
        value.trim() == "4194304",
        "hugetlb.2MB.max was {:?}, expected 4194304",
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
