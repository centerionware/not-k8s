use super::context::E2eContext;
use super::skip_test;
use anyhow::{Context, Result};
use k8s_openapi::api::core::v1::Pod;
use kube::api::{Api, PostParams};
use serde_json::json;
use std::fs;
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

fn run_privileged(program: &str, args: &[&str]) -> Result<()> {
    let uid = Command::new("id").arg("-u").output()?;
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

fn require_cri_and_systemd() -> Result<()> {
    if crate::config::Config::from_env()?.nodelet_runtime() != "cri" {
        return Err(skip_test("resource-manager checks require the CRI runtime"));
    }
    if !systemd_nodelet_available() {
        return Err(skip_test(
            "resource-manager checks need a systemd-managed nodelet.service",
        ));
    }
    if Command::new("id").arg("-u").output()?.stdout.starts_with(b"0")
        || Command::new("sudo").arg("-n").arg("true").status().is_ok_and(|status| status.success())
    {
        return Ok(());
    }
    Err(skip_test(
        "resource-manager checks need root or passwordless sudo to inspect cgroups",
    ))
}

fn wait_for_nodelet_server() -> Result<()> {
    let port = std::env::var("NODELET_SERVER_PORT")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(10250);
    let address = SocketAddr::from(([127, 0, 0, 1], port));
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if TcpStream::connect_timeout(&address, Duration::from_millis(250)).is_ok() {
            return Ok(());
        }
        if Instant::now() >= deadline {
            anyhow::bail!("nodelet service did not reopen its server on {address}");
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

pub(super) struct NodeletEnvOverride {
    drop_in: PathBuf,
}

impl NodeletEnvOverride {
    pub(super) fn install(env: &[(&str, &str)]) -> Result<Self> {
        let drop_in_dir = Path::new("/etc/systemd/system/nodelet.service.d");
        let drop_in = drop_in_dir.join(format!(
            "nodebootstrap-e2e-resource-manager-{}.conf",
            std::process::id()
        ));
        let local = std::env::temp_dir().join(format!(
            "nodebootstrap-e2e-resource-manager-{}.conf",
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
        fs::write(&local, contents)?;
        let drop_in_dir = drop_in_dir.to_string_lossy();
        let local = local.to_string_lossy();
        let drop_in_text = drop_in.to_string_lossy();
        let result = (|| {
            run_privileged("mkdir", &["-p", drop_in_dir.as_ref()])?;
            run_privileged("install", &["-m", "0644", local.as_ref(), drop_in_text.as_ref()])?;
            run_privileged("systemctl", &["daemon-reload"])?;
            run_privileged("systemctl", &["restart", "nodelet.service"])
        })();
        let _ = fs::remove_file(local.as_ref());
        result?;
        wait_for_nodelet_server()?;
        Ok(Self { drop_in })
    }
}

impl Drop for NodeletEnvOverride {
    fn drop(&mut self) {
        let drop_in = self.drop_in.to_string_lossy();
        let _ = run_privileged("rm", &["-f", drop_in.as_ref()]);
        let _ = run_privileged("systemctl", &["daemon-reload"]);
        let _ = run_privileged("systemctl", &["restart", "nodelet.service"]);
        let _ = wait_for_nodelet_server();
    }
}

async fn create_pod(context: &E2eContext, name: &str, resources: serde_json::Value) -> Result<()> {
    let pods: Api<Pod> = Api::namespaced(context.client.clone(), &context.namespace);
    let pod: Pod = serde_json::from_value(json!({
        "apiVersion": "v1",
        "kind": "Pod",
        "metadata": {"name": name},
        "spec": {
            "containers": [{
                "name": "app",
                "image": "busybox:latest",
                "command": ["sleep", "3600"],
                "resources": resources
            }]
        }
    }))?;
    pods.create(&PostParams::default(), &pod).await?;
    context
        .wait_until("resource-manager Pod to reach Running", Duration::from_secs(150), || {
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

async fn container_id(context: &E2eContext, name: &str) -> Result<String> {
    let pods: Api<Pod> = Api::namespaced(context.client.clone(), &context.namespace);
    context
        .wait_until("container ID to be reported", Duration::from_secs(60), || {
            let pods = pods.clone();
            async move {
                Ok(pods
                    .get(name)
                    .await?
                    .status
                    .and_then(|status| status.container_statuses)
                    .unwrap_or_default()
                    .into_iter()
                    .find_map(|status| status.container_id)
                    .is_some())
            }
        })
        .await?;
    pods.get(name)
        .await?
        .status
        .and_then(|status| status.container_statuses)
        .unwrap_or_default()
        .into_iter()
        .find_map(|status| status.container_id)
        .map(|id| id.rsplit_once(":").map_or(id.clone(), |(_, id)| id.to_string()))
        .context("Running container had no container ID")
}

fn find_cgroup(root: &Path, needle: &str, depth: usize) -> Option<PathBuf> {
    if depth == 0 {
        return None;
    }
    let entries = fs::read_dir(root).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.contains(needle))
            {
                return Some(path);
            }
            if let Some(found) = find_cgroup(&path, needle, depth - 1) {
                return Some(found);
            }
        }
    }
    None
}

fn cgroup_value(path: &Path, name: &str) -> Result<String> {
    fs::read_to_string(path.join(name))
        .with_context(|| format!("reading {}/{}", path.display(), name))
}

pub(super) async fn cpu_manager_pins_guaranteed_containers_to_disjoint_exclusive_cores(
    context: &E2eContext,
) -> Result<()> {
    require_cri_and_systemd()?;
    if std::thread::available_parallelism().map_or(1, |count| count.get()) < 2 {
        return Err(skip_test("CPU Manager exclusivity needs at least two CPUs"));
    }
    let _override = NodeletEnvOverride::install(&[("NODELET_CPU_MANAGER_POLICY", "static")])?;
    create_pod(
        context,
        "cpu-manager-exclusive-a",
        json!({"requests": {"cpu": "1", "memory": "64Mi"}, "limits": {"cpu": "1", "memory": "64Mi"}}),
    )
    .await?;
    create_pod(
        context,
        "cpu-manager-exclusive-b",
        json!({"requests": {"cpu": "1", "memory": "64Mi"}, "limits": {"cpu": "1", "memory": "64Mi"}}),
    )
    .await?;
    let root = Path::new("/sys/fs/cgroup");
    let a = find_cgroup(root, &container_id(context, "cpu-manager-exclusive-a").await?, 8)
        .ok_or_else(|| skip_test("could not find container A's cgroup directory"))?;
    let b = find_cgroup(root, &container_id(context, "cpu-manager-exclusive-b").await?, 8)
        .ok_or_else(|| skip_test("could not find container B's cgroup directory"))?;
    let cpuset_a = cgroup_value(&a, "cpuset.cpus")?;
    let cpuset_b = cgroup_value(&b, "cpuset.cpus")?;
    anyhow::ensure!(!cpuset_a.trim().is_empty(), "container A has no cpuset.cpus");
    anyhow::ensure!(!cpuset_b.trim().is_empty(), "container B has no cpuset.cpus");
    anyhow::ensure!(
        cpuset_a.trim() != cpuset_b.trim(),
        "two Guaranteed 1-CPU Pods received the same exclusive cpuset {cpuset_a:?}"
    );
    Ok(())
}

pub(super) async fn cpu_manager_retroactively_shrinks_an_already_running_shared_pool_container(
    context: &E2eContext,
) -> Result<()> {
    require_cri_and_systemd()?;
    if std::thread::available_parallelism().map_or(1, |count| count.get()) < 2 {
        return Err(skip_test("CPU Manager exclusivity needs at least two CPUs"));
    }
    let _override = NodeletEnvOverride::install(&[("NODELET_CPU_MANAGER_POLICY", "static")])?;
    create_pod(context, "cpu-manager-shared", json!({})).await?;
    let root = Path::new("/sys/fs/cgroup");
    let shared = find_cgroup(root, &container_id(context, "cpu-manager-shared").await?, 8)
        .ok_or_else(|| skip_test("could not find the shared container's cgroup directory"))?;
    let before = cgroup_value(&shared, "cpuset.cpus")?;
    create_pod(
        context,
        "cpu-manager-new-exclusive",
        json!({"requests": {"cpu": "1", "memory": "64Mi"}, "limits": {"cpu": "1", "memory": "64Mi"}}),
    )
    .await?;
    context
        .wait_until("shared-pool cpuset to shrink after an exclusive claim", Duration::from_secs(90), || {
            let shared = shared.clone();
            let before = before.clone();
            async move { Ok(cgroup_value(&shared, "cpuset.cpus").map_or(false, |value| value != before)) }
        })
        .await
}

pub(super) async fn memory_manager_pins_guaranteed_containers_to_a_numa_node(
    context: &E2eContext,
) -> Result<()> {
    require_cri_and_systemd()?;
    let _override = NodeletEnvOverride::install(&[("NODELET_MEMORY_MANAGER_POLICY", "static")])?;
    create_pod(
        context,
        "memory-manager-static",
        json!({"requests": {"cpu": "100m", "memory": "64Mi"}, "limits": {"cpu": "100m", "memory": "64Mi"}}),
    )
    .await?;
    let id = container_id(context, "memory-manager-static").await?;
    let path = find_cgroup(Path::new("/sys/fs/cgroup"), &id, 8)
        .ok_or_else(|| skip_test("could not find the memory-manager container's cgroup directory"))?;
    anyhow::ensure!(
        !cgroup_value(&path, "cpuset.mems")?.trim().is_empty(),
        "Guaranteed Pod has no cpuset.mems assignment"
    );
    Ok(())
}

fn has_rotated_log(path: &Path) -> bool {
    let uid = Command::new("id").arg("-u").output();
    let Ok(uid) = uid else { return false };
    let root = String::from_utf8_lossy(&uid.stdout).trim() == "0";
    let mut command = if root {
        let mut command = Command::new("find");
        command.args([path.to_string_lossy().as_ref(), "-maxdepth", "1", "-name", "app_*.log.1"]);
        command
    } else {
        let mut command = Command::new("sudo");
        command.args(["find", path.to_string_lossy().as_ref(), "-maxdepth", "1", "-name", "app_*.log.1"]);
        command
    };
    command.output().is_ok_and(|output| output.status.success() && !output.stdout.is_empty())
}

pub(super) async fn log_rotation_creates_a_rotated_file(context: &E2eContext) -> Result<()> {
    require_cri_and_systemd()?;
    let _override = NodeletEnvOverride::install(&[(
        "NODELET_CONTAINER_LOG_MAX_SIZE_BYTES",
        "4096",
    )])?;
    let name = "log-rotation";
    let pods: Api<Pod> = Api::namespaced(context.client.clone(), &context.namespace);
    let pod: Pod = serde_json::from_value(json!({
        "apiVersion": "v1",
        "kind": "Pod",
        "metadata": {"name": name},
        "spec": {"containers": [{"name": "app", "image": "busybox:latest", "command": ["sh", "-c", "while true; do echo 'log rotation check filler'; sleep 0.01; done"]}]}
    }))?;
    pods.create(&PostParams::default(), &pod).await?;
    context
        .wait_until("log rotation Pod to reach Running", Duration::from_secs(150), || {
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
    let pod = pods.get(name).await?;
    let uid = pod
        .metadata
        .uid
        .context("log rotation Pod has no UID")?;
    let log_dir = PathBuf::from(format!(
        "/var/log/pods/{}_{}_{}",
        context.namespace, name, uid
    ));
    context
        .wait_until("a rotated container log file", Duration::from_secs(90), || {
            let log_dir = log_dir.clone();
            async move { Ok(has_rotated_log(&log_dir)) }
        })
        .await
}
