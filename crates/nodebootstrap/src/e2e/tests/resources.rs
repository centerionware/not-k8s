use super::context::E2eContext;
use super::skip_test;
use anyhow::{Context, Result};
use k8s_openapi::api::core::v1::Pod;
use kube::api::{Api, AttachParams, Patch, PatchParams, PostParams};
use serde_json::json;
use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;
use tokio::io::AsyncReadExt;

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

async fn exec_output(context: &E2eContext, pod: &str, command: &[&str]) -> Result<String> {
    let pods: Api<Pod> = Api::namespaced(context.client.clone(), &context.namespace);
    let params = AttachParams::default()
        .container("app")
        .stdout(true)
        .stderr(false);
    let mut process = pods.exec(pod, command.iter().copied(), &params).await?;
    let mut stdout = Vec::new();
    if let Some(mut stream) = process.stdout() {
        stream.read_to_end(&mut stdout).await?;
    }
    process.join().await?;
    Ok(String::from_utf8_lossy(&stdout).into_owned())
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

pub(super) async fn in_place_resize_updates_memory_limit_without_restarting(
    context: &E2eContext,
) -> Result<()> {
    if crate::config::Config::from_env()?.nodelet_runtime() != "cri" {
        return Err(skip_test("in-place resize checks require the CRI runtime"));
    }
    let name = "in-place-resize";
    create_pod(
        context,
        name,
        json!({
            "containers": [{
                "name": "app",
                "image": "busybox:latest",
                "command": ["sleep", "3600"],
                "resources": {"limits": {"memory": "134217728"}}
            }]
        }),
    )
    .await?;
    let pods: Api<Pod> = Api::namespaced(context.client.clone(), &context.namespace);
    context
        .wait_until("resize Pod to reach Running", Duration::from_secs(90), || {
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
    let before = exec_output(context, name, &["cat", "/sys/fs/cgroup/memory.max"]).await?;
    if before.trim().is_empty() {
        return Err(skip_test(
            "the resize Pod could not read /sys/fs/cgroup/memory.max; this node may use cgroup v1",
        ));
    }
    anyhow::ensure!(
        before.trim() == "134217728",
        "initial memory.max was {:?}, expected 134217728",
        before.trim()
    );
    pods.patch_resize(
        name,
        &PatchParams::default(),
        &Patch::Merge(json!({"spec":{"containers":[{"name":"app","resources":{"limits":{"memory":"268435456"}}}]}})),
    )
    .await
    .context("the apiserver must support the Pod resize subresource")?;
    context
        .wait_until("container memory.max to reflect the in-place resize", Duration::from_secs(60), || {
            let context = context.clone();
            async move {
                Ok(exec_output(&context, name, &["cat", "/sys/fs/cgroup/memory.max"])
                    .await
                    .is_ok_and(|output| output.trim() == "268435456"))
            }
        })
        .await?;
    let status = pods.get(name).await?.status.context("resize Pod has no status")?;
    let container = status
        .container_statuses
        .unwrap_or_default()
        .into_iter()
        .find(|container| container.name == "app")
        .context("resize Pod has no app container status")?;
    anyhow::ensure!(
        container.restart_count == 0,
        "in-place resize restarted the container {} times",
        container.restart_count
    );
    Ok(())
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

fn privileged(program: &str, args: &[&str]) -> Result<()> {
    let uid = Command::new("id").arg("-u").output()?;
    let mut command = if String::from_utf8_lossy(&uid.stdout).trim() == "0" {
        let mut command = Command::new(program);
        command.args(args);
        command
    } else {
        let mut command = Command::new("sudo");
        command.arg(program).args(args);
        command
    };
    let output = command.output()?;
    anyhow::ensure!(
        output.status.success(),
        "{program} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(())
}

struct TemporarySwapFile(Option<PathBuf>);

impl Drop for TemporarySwapFile {
    fn drop(&mut self) {
        let Some(path) = self.0.take() else {
            return;
        };
        let path = path.to_string_lossy();
        let _ = privileged("swapoff", &[path.as_ref()]);
        let _ = privileged("rm", &["-f", path.as_ref()]);
    }
}

pub(super) async fn limited_swap_gives_burstable_pods_proportional_swap(
    context: &E2eContext,
) -> Result<()> {
    if crate::config::Config::from_env()?.nodelet_runtime() != "cri" {
        return Err(skip_test("LimitedSwap checks require the CRI runtime"));
    }
    if !Command::new("systemctl")
        .args(["show", "nodelet.service", "--property=LoadState", "--value"])
        .output()
        .is_ok_and(|output| {
            output.status.success() && String::from_utf8_lossy(&output.stdout).trim() == "loaded"
        })
    {
        return Err(skip_test(
            "LimitedSwap checks require a systemd-managed nodelet.service",
        ));
    }

    let swap_total = fs::read_to_string("/proc/meminfo")
        .ok()
        .and_then(|contents| {
            contents
                .lines()
                .find(|line| line.starts_with("SwapTotal:"))
                .and_then(|line| line.split_whitespace().nth(1))
                .and_then(|value| value.parse::<u64>().ok())
        })
        .unwrap_or_default();
    let temporary_swap = if swap_total == 0 {
        let path = std::env::temp_dir().join(format!(
            "nodebootstrap-e2e-swap-{}",
            std::process::id()
        ));
        let path_string = path.to_string_lossy().to_string();
        if privileged("fallocate", &["-l", "512M", &path_string]).is_err()
            && privileged(
                "dd",
                &[
                    "if=/dev/zero",
                    &format!("of={path_string}"),
                    "bs=1M",
                    "count=512",
                    "status=none",
                ],
            )
            .is_err()
        {
            let _ = privileged("rm", &["-f", &path_string]);
            return Err(skip_test(
                "could not create a temporary swapfile; LimitedSwap needs real swap",
            ));
        }
        let enabled = privileged("chmod", &["600", &path_string])
            .and_then(|_| privileged("mkswap", &[&path_string]))
            .and_then(|_| privileged("swapon", &[&path_string]));
        if enabled.is_err() {
            let _ = privileged("rm", &["-f", &path_string]);
            return Err(skip_test(
                "could not enable a temporary swapfile; this host does not permit swapon",
            ));
        }
        TemporarySwapFile(Some(path))
    } else {
        TemporarySwapFile(None)
    };
    let _nodelet_env = match super::resource_managers::NodeletEnvOverride::install(&[
        ("NODELET_MEMORY_SWAP_BEHAVIOR", "LimitedSwap"),
    ]) {
        Ok(override_guard) => override_guard,
        Err(error) => {
            drop(temporary_swap);
            return Err(skip_test(format!(
                "could not restart nodelet with LimitedSwap: {error}"
            )));
        }
    };

    let burstable = "limited-swap-burstable";
    create_pod(
        context,
        burstable,
        json!({
            "restartPolicy": "Never",
            "containers": [{
                "name": "app",
                "image": "busybox:latest",
                "command": ["sleep", "3600"],
                "resources": {"requests": {"memory": "64Mi"}, "limits": {"memory": "256Mi"}}
            }]
        }),
    )
    .await?;
    let burstable_swap = exec_output(context, burstable, &["cat", "/sys/fs/cgroup/memory.swap.max"])
        .await
        .map_err(|error| skip_test(format!("memory.swap.max is unavailable; this host may use cgroup v1: {error}")))?;
    let burstable_swap = burstable_swap.trim();
    let burstable_swap = burstable_swap
        .parse::<u64>()
        .map_err(|_| skip_test(format!("memory.swap.max was not numeric: {burstable_swap:?}")))?;
    anyhow::ensure!(
        burstable_swap > 0,
        "a bounded Burstable Pod under LimitedSwap should get a nonzero memory.swap.max"
    );

    let guaranteed = "limited-swap-guaranteed";
    create_pod(
        context,
        guaranteed,
        json!({
            "restartPolicy": "Never",
            "containers": [{
                "name": "app",
                "image": "busybox:latest",
                "command": ["sleep", "3600"],
                "resources": {
                    "requests": {"memory": "64Mi", "cpu": "100m"},
                    "limits": {"memory": "64Mi", "cpu": "100m"}
                }
            }]
        }),
    )
    .await?;
    let guaranteed_swap =
        exec_output(context, guaranteed, &["cat", "/sys/fs/cgroup/memory.swap.max"]).await?;
    anyhow::ensure!(
        guaranteed_swap.trim() == "0",
        "a Guaranteed Pod should have memory.swap.max=0 under LimitedSwap, got {:?}",
        guaranteed_swap.trim()
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
