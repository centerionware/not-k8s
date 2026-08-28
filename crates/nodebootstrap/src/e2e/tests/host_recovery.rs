use super::context::E2eContext;
use super::skip_test;
use anyhow::{Context, Result};
use k8s_openapi::api::core::v1::{Node, Pod};
use kube::api::{Api, ListParams, PostParams};
use serde_json::json;
use std::process::Command;
use std::time::Duration;

fn systemctl(action: &str, unit: &str) -> Result<()> {
    systemctl_args(&[action, unit])
}

fn systemctl_args(args: &[&str]) -> Result<()> {
    let uid = Command::new("id").arg("-u").output()?;
    let uid = String::from_utf8_lossy(&uid.stdout).trim().to_string();
    let output = if uid == "0" {
        Command::new("systemctl").args(args).output()?
    } else {
        Command::new("sudo")
            .args(["systemctl"])
            .args(args)
            .output()?
    };
    anyhow::ensure!(
        output.status.success(),
        "systemctl {:?} failed: {}",
        args,
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(())
}

fn containerd_is_up() -> bool {
    Command::new("systemctl")
        .args(["is-active", "--quiet", "containerd"])
        .status()
        .is_ok_and(|status| status.success())
        && std::path::Path::new("/run/containerd/containerd.sock").exists()
}

fn nodelet_is_up() -> bool {
    Command::new("systemctl")
        .args(["is-active", "--quiet", "nodelet.service"])
        .status()
        .is_ok_and(|status| status.success())
}

pub(super) async fn pending_pod_recovers_after_the_node_failure_is_fixed(
    context: &E2eContext,
) -> Result<()> {
    if crate::config::Config::from_env()?.nodelet_runtime() != "cri" {
        return Err(skip_test("retry recovery requires the CRI runtime"));
    }
    if !Command::new("systemctl")
        .args(["is-active", "--quiet", "containerd"])
        .status()
        .is_ok_and(|status| status.success())
    {
        return Err(skip_test("containerd is not a running systemd unit"));
    }
    let nodes: Api<Node> = Api::all(context.client.clone());
    let node_name = nodes
        .list(&ListParams::default())
        .await?
        .items
        .into_iter()
        .find_map(|node| node.metadata.name)
        .context("cluster has no Node to pin the recovery Pod to")?;
    let name = "retry-after-repair";
    let pods: Api<Pod> = Api::namespaced(context.client.clone(), &context.namespace);
    systemctl("stop", "containerd")?;
    let pod: Pod = serde_json::from_value(json!({
        "apiVersion": "v1",
        "kind": "Pod",
        "metadata": {"name": name},
        "spec": {
            "nodeName": node_name,
            "containers": [{"name": "app", "image": "busybox:latest", "command": ["sleep", "3600"]}]
        }
    }))?;
    let create_result = pods.create(&PostParams::default(), &pod).await;
    if let Err(error) = create_result {
        let _ = systemctl("start", "containerd");
        return Err(error.into());
    }
    let not_running = context
        .wait_until("recovery Pod to stay down while containerd is stopped", Duration::from_secs(30), || {
            let pods = pods.clone();
            async move {
                Ok(pods
                    .get(name)
                    .await?
                    .status
                    .and_then(|status| status.phase)
                    .as_deref()
                    != Some("Running"))
            }
        })
        .await;
    let started = systemctl("start", "containerd");
    not_running?;
    started?;
    context
        .wait_until("containerd socket to recover", Duration::from_secs(60), || async {
            Ok(containerd_is_up())
        })
        .await?;
    context
        .wait_until("recovery Pod to reach Running after containerd repair", Duration::from_secs(300), || {
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
        .await
}

pub(super) async fn existing_pod_recreates_its_container_after_a_runtime_restart(
    context: &E2eContext,
) -> Result<()> {
    if crate::config::Config::from_env()?.nodelet_runtime() != "cri" {
        return Err(skip_test("runtime inventory recovery requires the CRI runtime"));
    }
    if !containerd_is_up() || !nodelet_is_up() {
        return Err(skip_test(
            "runtime inventory recovery requires systemd-managed containerd and nodelet",
        ));
    }

    let nodes: Api<Node> = Api::all(context.client.clone());
    let node_name = nodes
        .list(&ListParams::default())
        .await?
        .items
        .into_iter()
        .find_map(|node| node.metadata.name)
        .context("cluster has no Node to pin the recovery Pod to")?;
    let name = "runtime-inventory-recovery";
    let pods: Api<Pod> = Api::namespaced(context.client.clone(), &context.namespace);
    let pod: Pod = serde_json::from_value(json!({
        "apiVersion": "v1",
        "kind": "Pod",
        "metadata": {"name": name},
        "spec": {
            "nodeName": node_name,
            "containers": [{"name": "app", "image": "busybox:latest", "command": ["sleep", "3600"]}]
        }
    }))?;
    pods.create(&PostParams::default(), &pod).await?;
    context
        .wait_until(
            "runtime inventory test Pod to reach Running",
            Duration::from_secs(120),
            || {
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
            },
        )
        .await?;
    let before = pods.get(name).await?;
    let old_container_id = before
        .status
        .and_then(|status| status.container_statuses)
        .and_then(|statuses| statuses.into_iter().next())
        .and_then(|status| status.container_id)
        .context("Running Pod has no container ID before runtime restart")?;

    let result = async {
        // Stop nodelet before taking the runtime down, leaving the API's old
        // Running status in place. Killing the complete containerd unit cgroup
        // models a reboot more closely than a graceful service restart: the
        // task disappears while CRI's persisted metadata remains to be
        // reconciled when both services return.
        systemctl("stop", "nodelet.service")?;
        systemctl_args(&[
            "kill",
            "--kill-who=all",
            "--signal=SIGKILL",
            "containerd.service",
        ])?;
        let _ = systemctl("stop", "containerd.service");
        context
            .wait_until(
                "containerd to be down for the runtime restart",
                Duration::from_secs(30),
                || async { Ok(!containerd_is_up()) },
            )
            .await?;

        systemctl("start", "containerd.service")?;
        context
            .wait_until(
                "containerd to recover for runtime inventory",
                Duration::from_secs(60),
                || async { Ok(containerd_is_up()) },
            )
            .await?;
        systemctl("start", "nodelet.service")?;
        context
            .wait_until(
                "nodelet to recover for runtime inventory",
                Duration::from_secs(60),
                || async { Ok(nodelet_is_up()) },
            )
            .await?;

        context
            .wait_until(
                "the existing Pod to get a newly reconciled container",
                Duration::from_secs(180),
                || {
                    let pods = pods.clone();
                    let old_container_id = old_container_id.clone();
                    async move {
                        let Some(status) = pods.get(name).await?.status else {
                            return Ok(false);
                        };
                        let Some(container) = status
                            .container_statuses
                            .and_then(|statuses| statuses.into_iter().next())
                        else {
                            return Ok(false);
                        };
                        Ok(status.phase.as_deref() == Some("Running")
                            && container.ready
                            && container
                                .container_id
                                .is_some_and(|id| id != old_container_id))
                    }
                },
            )
            .await
    }
    .await;

    // Leave the shared e2e host usable even if the recovery assertions fail
    // halfway through the simulated reboot.
    let _ = systemctl("start", "containerd.service");
    let _ = systemctl("start", "nodelet.service");
    result
}
