use super::context::E2eContext;
use super::skip_test;
use anyhow::{Context, Result};
use k8s_openapi::api::core::v1::Node;
use kube::api::{Api, ListParams};
use serde_json::Value;
use std::process::Command;
use std::time::Duration;

fn nodecontroller_is_active() -> bool {
    Command::new("systemctl")
        .args(["is-active", "--quiet", "nodecontroller"])
        .status()
        .is_ok_and(|status| status.success())
        || Command::new("pgrep")
            .args(["-x", "nodecontroller"])
            .status()
            .is_ok_and(|status| status.success())
}

fn require_nodecontroller() -> Result<()> {
    if nodecontroller_is_active() {
        Ok(())
    } else {
        Err(skip_test(
            "nodecontroller is not active; bootstrap with --controller-manager=nodecontroller to exercise the replacement controller manager",
        ))
    }
}

fn systemctl(action: &str, unit: &str) -> Result<()> {
    let uid = Command::new("id")
        .arg("-u")
        .output()
        .context("checking the e2e runner's uid")?;
    let uid = String::from_utf8_lossy(&uid.stdout).trim().to_string();
    let mut command = if uid == "0" {
        let mut command = Command::new("systemctl");
        command.args([action, unit]);
        command
    } else {
        let mut command = Command::new("sudo");
        command.args(["systemctl", action, unit]);
        command
    };
    let output = command
        .output()
        .with_context(|| format!("running systemctl {action} {unit}"))?;
    anyhow::ensure!(
        output.status.success(),
        "systemctl {action} {unit} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(())
}

async fn node_has_taint(nodes: &Api<Node>, name: &str, key: &str) -> Result<bool> {
    let node = nodes.get(name).await?;
    Ok(serde_json::to_value(node)?
        .pointer("/spec/taints")
        .and_then(Value::as_array)
        .is_some_and(|taints| {
            taints.iter().any(|taint| {
                taint.get("key").and_then(Value::as_str) == Some(key)
            })
        }))
}

async fn node_ready(nodes: &Api<Node>, name: &str) -> Result<bool> {
    let node = nodes.get(name).await?;
    Ok(serde_json::to_value(node)?
        .pointer("/status/conditions")
        .and_then(Value::as_array)
        .is_some_and(|conditions| {
            conditions.iter().any(|condition| {
                condition.get("type").and_then(Value::as_str) == Some("Ready")
                    && condition.get("status").and_then(Value::as_str) == Some("True")
            })
        }))
}

pub(super) async fn node_is_tainted_unreachable_after_heartbeat_loss_and_recovers(
    context: &E2eContext,
) -> Result<()> {
    require_nodecontroller()?;
    if !Command::new("systemctl")
        .args(["list-unit-files", "nodelet.service"])
        .status()
        .is_ok_and(|status| status.success())
    {
        return Err(skip_test(
            "nodelet.service is unavailable; heartbeat-loss recovery needs systemd to stop and start nodelet",
        ));
    }
    let nodes: Api<Node> = Api::all(context.client.clone());
    let name = nodes
        .list(&ListParams::default())
        .await?
        .items
        .into_iter()
        .find_map(|node| node.metadata.name)
        .context("the cluster has no Node to monitor")?;
    let taint_key = "node.kubernetes.io/unreachable";

    systemctl("stop", "nodelet.service")?;
    let tainted = context
        .wait_until(
            "node to receive the unreachable taint after its heartbeat expires",
            Duration::from_secs(90),
            || {
                let nodes = nodes.clone();
                let name = name.clone();
                async move { node_has_taint(&nodes, &name, taint_key).await }
            },
        )
        .await;
    let started = systemctl("start", "nodelet.service");
    tainted?;
    started?;

    context
        .wait_until("node to become Ready after nodelet restarts", Duration::from_secs(120), || {
            let nodes = nodes.clone();
            let name = name.clone();
            async move { node_ready(&nodes, &name).await }
        })
        .await?;
    context
        .wait_until("unreachable taint to clear after heartbeat recovery", Duration::from_secs(60), || {
            let nodes = nodes.clone();
            let name = name.clone();
            async move { Ok(!node_has_taint(&nodes, &name, taint_key).await?) }
        })
        .await
}
