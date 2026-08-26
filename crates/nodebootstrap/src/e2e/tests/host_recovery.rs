use super::context::E2eContext;
use super::skip_test;
use anyhow::{Context, Result};
use k8s_openapi::api::core::v1::{Node, Pod};
use kube::api::{Api, ListParams, PostParams};
use serde_json::json;
use std::process::Command;
use std::time::Duration;

fn systemctl(action: &str, unit: &str) -> Result<()> {
    let uid = Command::new("id").arg("-u").output()?;
    let uid = String::from_utf8_lossy(&uid.stdout).trim().to_string();
    let output = if uid == "0" {
        Command::new("systemctl").args([action, unit]).output()?
    } else {
        Command::new("sudo")
            .args(["systemctl", action, unit])
            .output()?
    };
    anyhow::ensure!(
        output.status.success(),
        "systemctl {action} {unit} failed: {}",
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
