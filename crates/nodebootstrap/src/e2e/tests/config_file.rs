use super::context::E2eContext;
use super::skip_test;
use anyhow::{Context, Result};
use k8s_openapi::api::core::v1::Node;
use kube::api::{Api, ListParams};
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

fn run_privileged(program: &str, args: &[&str]) -> Result<()> {
    let uid = Command::new("id")
        .arg("-u")
        .output()
        .context("checking the e2e runner's uid")?;
    let uid = String::from_utf8_lossy(&uid.stdout).trim().to_string();
    let mut command = if uid == "0" {
        let mut command = Command::new(program);
        command.args(args);
        command
    } else {
        let mut command = Command::new("sudo");
        command.arg(program).args(args);
        command
    };
    let output = command
        .output()
        .with_context(|| format!("running {program}"))?;
    anyhow::ensure!(
        output.status.success(),
        "{program} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(())
}

fn systemd_nodelet_available() -> bool {
    Command::new("systemctl")
        .args(["show", "nodelet.service", "--property=LoadState", "--value"])
        .output()
        .is_ok_and(|output| {
            output.status.success() && String::from_utf8_lossy(&output.stdout).trim() == "loaded"
        })
}

struct NodeletConfigOverride {
    drop_in: PathBuf,
    files: Vec<PathBuf>,
}

impl NodeletConfigOverride {
    fn install(env: &[(&str, &str)], files: Vec<PathBuf>) -> Result<Self> {
        if !systemd_nodelet_available() {
            return Err(skip_test(
                "nodelet.service is unavailable; config-file e2e checks need systemd",
            ));
        }

        let drop_in_dir = Path::new("/etc/systemd/system/nodelet.service.d");
        let drop_in = drop_in_dir.join(format!(
            "nodebootstrap-e2e-{}.conf",
            std::process::id()
        ));
        let local_drop_in = std::env::temp_dir().join(format!(
            "nodebootstrap-e2e-{}.conf",
            std::process::id()
        ));
        let mut contents = String::from("[Service]\n");
        for (key, value) in env {
            contents.push_str("Environment=");
            contents.push_str(key);
            contents.push('=');
            contents.push_str(value);
            contents.push('\n');
        }
        fs::write(&local_drop_in, contents)
            .with_context(|| format!("writing {}", local_drop_in.display()))?;

        let guard = Self { drop_in, files };
        let drop_in_dir = drop_in_dir.to_string_lossy();
        let local_drop_in = local_drop_in.to_string_lossy();
        let drop_in = guard.drop_in.to_string_lossy();
        run_privileged("mkdir", &["-p", drop_in_dir.as_ref()])?;
        run_privileged(
            "install",
            &["-m", "0644", local_drop_in.as_ref(), drop_in.as_ref()],
        )?;
        run_privileged("systemctl", &["daemon-reload"])?;
        run_privileged("systemctl", &["restart", "nodelet.service"])?;
        let _ = fs::remove_file(local_drop_in.as_ref());
        Ok(guard)
    }
}

impl Drop for NodeletConfigOverride {
    fn drop(&mut self) {
        let drop_in = self.drop_in.to_string_lossy();
        let _ = run_privileged("rm", &["-f", drop_in.as_ref()]);
        let _ = run_privileged("systemctl", &["daemon-reload"]);
        let _ = run_privileged("systemctl", &["restart", "nodelet.service"]);
        for file in &self.files {
            let _ = fs::remove_file(file);
            let _ = fs::remove_dir(file);
        }
    }
}

fn temporary_path(label: &str) -> PathBuf {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    std::env::temp_dir().join(format!("nodelet-config-{label}-{}-{stamp}", std::process::id()))
}

fn require_cri_and_systemd() -> Result<()> {
    if crate::config::Config::from_env()?.nodelet_runtime() != "cri" {
        return Err(skip_test("config-file checks require the CRI runtime"));
    }
    if !systemd_nodelet_available() {
        return Err(skip_test(
            "nodelet.service is unavailable; config-file e2e checks need systemd",
        ));
    }
    Ok(())
}

async fn first_node(context: &E2eContext) -> Result<(Api<Node>, String)> {
    let nodes: Api<Node> = Api::all(context.client.clone());
    let name = nodes
        .list(&ListParams::default())
        .await?
        .items
        .into_iter()
        .find_map(|node| node.metadata.name)
        .context("the cluster has no Node to inspect")?;
    Ok((nodes, name))
}

async fn node_pod_capacity(nodes: &Api<Node>, name: &str, expected: &str) -> Result<bool> {
    let node = nodes.get(name).await?;
    Ok(serde_json::to_value(node)?
        .pointer("/status/capacity/pods")
        .and_then(Value::as_str)
        == Some(expected))
}

async fn node_is_ready(nodes: &Api<Node>, name: &str) -> Result<bool> {
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

pub(super) async fn config_file_sets_a_value_env_did_not_override(
    context: &E2eContext,
) -> Result<()> {
    require_cri_and_systemd()?;
    let (nodes, node_name) = first_node(context).await?;
    let config = temporary_path("file");
    fs::write(&config, "NODELET_MAX_PODS: 42\n")?;
    let config = config.to_string_lossy().into_owned();
    let _override = NodeletConfigOverride::install(
        &[("NODELET_CONFIG_FILE", config.as_str())],
        vec![PathBuf::from(&config)],
    )?;
    let result = context
        .wait_until("Node.status.capacity.pods from NODELET_CONFIG_FILE", Duration::from_secs(90), || {
            let nodes = nodes.clone();
            let node_name = node_name.clone();
            async move { node_pod_capacity(&nodes, &node_name, "42").await }
        })
        .await;
    drop(_override);
    let restored = context
        .wait_until("nodelet to become Ready after config-file cleanup", Duration::from_secs(120), || {
            let nodes = nodes.clone();
            let node_name = node_name.clone();
            async move { node_is_ready(&nodes, &node_name).await }
        })
        .await;
    result?;
    restored
}

pub(super) async fn config_file_precedence_a_real_env_var_still_wins(
    context: &E2eContext,
) -> Result<()> {
    require_cri_and_systemd()?;
    let (nodes, node_name) = first_node(context).await?;
    let config = temporary_path("precedence");
    fs::write(&config, "NODELET_MAX_PODS: 42\n")?;
    let config = config.to_string_lossy().into_owned();
    let _override = NodeletConfigOverride::install(
        &[
            ("NODELET_CONFIG_FILE", config.as_str()),
            ("NODELET_MAX_PODS", "10"),
        ],
        vec![PathBuf::from(&config)],
    )?;
    let result = context
        .wait_until("environment precedence over NODELET_CONFIG_FILE", Duration::from_secs(90), || {
            let nodes = nodes.clone();
            let node_name = node_name.clone();
            async move { node_pod_capacity(&nodes, &node_name, "10").await }
        })
        .await;
    drop(_override);
    let restored = context
        .wait_until("nodelet to become Ready after config-file cleanup", Duration::from_secs(120), || {
            let nodes = nodes.clone();
            let node_name = node_name.clone();
            async move { node_is_ready(&nodes, &node_name).await }
        })
        .await;
    result?;
    restored
}

pub(super) async fn config_dir_merges_files_in_filename_order(
    context: &E2eContext,
) -> Result<()> {
    require_cri_and_systemd()?;
    let (nodes, node_name) = first_node(context).await?;
    let config_dir = temporary_path("dir");
    fs::create_dir_all(&config_dir)?;
    fs::write(config_dir.join("00-base.yaml"), "NODELET_MAX_PODS: 42\n")?;
    fs::write(config_dir.join("01-override.yaml"), "NODELET_MAX_PODS: 77\n")?;
    let config_dir = config_dir.to_string_lossy().into_owned();
    let _override = NodeletConfigOverride::install(
        &[("NODELET_CONFIG_DIR", config_dir.as_str())],
        vec![PathBuf::from(&config_dir)],
    )?;
    let result = context
        .wait_until("filename-ordered NODELET_CONFIG_DIR merge", Duration::from_secs(90), || {
            let nodes = nodes.clone();
            let node_name = node_name.clone();
            async move { node_pod_capacity(&nodes, &node_name, "77").await }
        })
        .await;
    drop(_override);
    let restored = context
        .wait_until("nodelet to become Ready after config-directory cleanup", Duration::from_secs(120), || {
            let nodes = nodes.clone();
            let node_name = node_name.clone();
            async move { node_is_ready(&nodes, &node_name).await }
        })
        .await;
    result?;
    restored
}
