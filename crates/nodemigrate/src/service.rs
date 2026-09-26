use std::process::{Command, Output};
use std::time::{Duration, Instant};
#[cfg(unix)]
use std::{os::unix::fs::FileTypeExt, path::Path};

use anyhow::{bail, ensure, Context, Result};

use crate::{
    detect::{Installation, NodeRole, ServiceManager},
    request::Distribution,
};

#[derive(Debug, Clone)]
pub struct PreviousServiceState {
    enabled: bool,
    active: bool,
    services: Vec<ServiceState>,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct SourceCiliumIdentity {
    container_ids: Vec<String>,
    pod_uids: Vec<String>,
}

#[derive(Debug, Clone)]
struct ServiceState {
    name: String,
    enabled: bool,
    active: bool,
}

pub fn require_supported_uninstall(installation: &Installation) -> Result<()> {
    match installation.distribution {
        crate::request::Distribution::K3s => ensure!(
            std::path::Path::new("/usr/local/bin/k3s-uninstall.sh").is_file(),
            "uninstall-after-migrate=true requires /usr/local/bin/k3s-uninstall.sh"
        ),
        crate::request::Distribution::Kubernetes => ensure!(
            installation
                .binary
                .as_ref()
                .is_some_and(|path| path.file_name().is_some_and(|name| name == "kubeadm")),
            "uninstall-after-migrate=true requires a detected kubeadm installation"
        ),
        crate::request::Distribution::Nodestore => ensure!(
            find_nodebootstrap().is_some(),
            "uninstall-after-migrate=true requires nodebootstrap or notk8s on PATH or beside nodemigrate"
        ),
    }
    Ok(())
}

pub fn validate_disable_support(installation: &Installation) -> Result<()> {
    ensure!(
        installation.service_manager.is_some(),
        "migration requires a detected service manager so the source can be stopped and restored"
    );
    Ok(())
}

pub fn disable(installation: &Installation) -> Result<PreviousServiceState> {
    let manager = installation
        .service_manager
        .context("source service manager could not be identified; refusing to stop the source")?;
    let name = &installation.service_name;
    if installation.distribution == crate::request::Distribution::Nodestore {
        return stop_nodestore_stack(manager, true);
    }
    let previous = match manager {
        ServiceManager::Systemd => {
            let enabled = command("systemctl", &["is-enabled", &format!("{name}.service")])
                .is_ok_and(|output| output.status.success());
            let active = command("systemctl", &["is-active", &format!("{name}.service")])
                .is_ok_and(|output| output.status.success());
            PreviousServiceState {
                enabled,
                active,
                services: Vec::new(),
            }
        }
        ServiceManager::OpenRc => {
            let default_runlevel = std::path::Path::new("/etc/runlevels/default")
                .join(name)
                .exists();
            let active = command("rc-service", &[name, "status"])
                .is_ok_and(|output| output.status.success());
            PreviousServiceState {
                enabled: default_runlevel,
                active,
                services: Vec::new(),
            }
        }
        ServiceManager::SysVInit => PreviousServiceState {
            enabled: sysv_enabled(name),
            active: command("service", &[name, "status"])
                .is_ok_and(|output| output.status.success()),
            services: Vec::new(),
        },
        ServiceManager::Runit => {
            let service_dir = runit_service_dir(name);
            PreviousServiceState {
                enabled: service_dir
                    .as_ref()
                    .is_some_and(|dir| !dir.join("down").exists()),
                active: service_dir.as_ref().is_some_and(|dir| {
                    command("sv", &["status", &dir.to_string_lossy()]).is_ok_and(|output| {
                        output.status.success()
                            && String::from_utf8_lossy(&output.stdout).starts_with("run:")
                    })
                }),
                services: Vec::new(),
            }
        }
    };
    ensure!(
        previous.active,
        "source service '{}' is not active; refusing to migrate a stopped cluster",
        installation.service_name
    );
    if let Err(error) = stop_and_disable(manager, name) {
        if let Err(restore_error) = restore(installation, previous) {
            bail!("disabling source service failed ({error:#}) and restoring its previous state failed ({restore_error:#})");
        }
        return Err(error).context("could not disable source service; previous state was restored");
    }
    Ok(previous)
}

/// Remove kubeadm control-plane static-pod sandboxes after kubelet is stopped.
/// Kubelet shutdown alone leaves those CRI containers bound to API/etcd ports.
pub fn stop_upstream_static_pods(installation: &Installation) -> Result<()> {
    if installation.distribution != Distribution::Kubernetes
        || installation.role != NodeRole::ControlPlane
    {
        return Ok(());
    }
    let endpoint = std::env::var("NODEMIGRATE_CRI_ENDPOINT")
        .ok()
        .filter(|value| !value.is_empty())
        .or_else(|| installation.runtime_endpoint.clone())
        .unwrap_or_else(|| "unix:///run/containerd/containerd.sock".to_string());
    let output = command(
        "crictl",
        &["--runtime-endpoint", &endpoint, "pods", "-o", "json"],
    )
    .context("listing source static pods; install crictl or set NODEMIGRATE_CRI_ENDPOINT")?;
    ensure!(
        output.status.success(),
        "crictl could not list source pod sandboxes: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    let pods: serde_json::Value =
        serde_json::from_slice(&output.stdout).context("parsing CRI pod sandbox list")?;
    let ids = static_pod_sandbox_ids(&pods);
    ensure!(
        !ids.is_empty(),
        "no kubeadm control-plane static pod sandboxes were found after stopping kubelet"
    );
    tracing::info!(count = ids.len(), "stopping kubeadm static pod sandboxes");
    for id in &ids {
        checked("crictl", &["--runtime-endpoint", &endpoint, "stopp", id])
            .with_context(|| format!("stopping source static pod sandbox {id}"))?;
        checked("crictl", &["--runtime-endpoint", &endpoint, "rmp", id])
            .with_context(|| format!("removing source static pod sandbox {id}"))?;
    }
    wait_for_api_port_release()
}

/// Remove CRI pod sandboxes left behind after stopping the source Kubernetes
/// service. Stopping kubelet/K3s does not stop existing containers. Leaving
/// them running lets the new node agent start a second copy of the migrated
/// Pods, which can collide on host ports, sockets, and mounted data.
pub fn stop_source_pod_sandboxes(installation: &Installation) -> Result<SourceCiliumIdentity> {
    let endpoint = std::env::var("NODEMIGRATE_CRI_ENDPOINT")
        .ok()
        .filter(|value| !value.is_empty())
        .or_else(|| installation.runtime_endpoint.clone())
        .unwrap_or_else(|| "unix:///run/containerd/containerd.sock".to_string());
    let output = command(
        "crictl",
        &["--runtime-endpoint", &endpoint, "pods", "-o", "json"],
    )
    .context("listing source pod sandboxes; install crictl or set NODEMIGRATE_CRI_ENDPOINT")?;
    ensure!(
        output.status.success(),
        "crictl could not list source pod sandboxes: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    let pods: serde_json::Value =
        serde_json::from_slice(&output.stdout).context("parsing CRI pod sandbox list")?;
    let (ready, all, cilium_pod_uids) = source_pod_sandbox_ids(&pods);
    tracing::info!(
        count = all.len(),
        runtime_endpoint = %endpoint,
        "stopping source pod sandboxes before cutover"
    );
    let mut stop_failures = Vec::new();
    for id in &ready {
        if let Err(error) = checked("crictl", &["--runtime-endpoint", &endpoint, "stopp", id]) {
            stop_failures.push((id.clone(), format!("{error:#}")));
        }
    }
    let output = command(
        "crictl",
        &["--runtime-endpoint", &endpoint, "ps", "-a", "-o", "json"],
    )
    .context("checking for running source containers after stopping pod sandboxes")?;
    ensure!(
        output.status.success(),
        "crictl could not list source containers: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    let containers: serde_json::Value =
        serde_json::from_slice(&output.stdout).context("parsing CRI container list")?;
    let running = running_sandbox_ids(&containers);
    ensure_failed_stops_are_inactive(&stop_failures, &running)?;
    for (id, error) in stop_failures {
        tracing::warn!(sandbox_id = %id, error = %error, "source sandbox stop failed but CRI confirms it has no running containers");
    }
    let cilium_host_containers = cilium_host_container_ids(&containers);
    if !cilium_host_containers.is_empty() || !cilium_pod_uids.is_empty() {
        eprintln!(
            "nodemigrate: source Cilium host-process identities: CRI containers [{}], pod UIDs [{}]",
            cilium_host_containers.join(","),
            cilium_pod_uids.join(",")
        );
    }
    for id in &all {
        if let Err(error) = checked("crictl", &["--runtime-endpoint", &endpoint, "rmp", id]) {
            ensure!(
                !running.contains(id),
                "removing source pod sandbox {id} failed while its container is still running: {error:#}"
            );
            tracing::warn!(sandbox_id = id, error = %error, "stopped source sandbox could not be removed; continuing with no running containers");
        }
    }
    Ok(SourceCiliumIdentity {
        container_ids: cilium_host_containers,
        pod_uids: cilium_pod_uids,
    })
}

/// Source Cilium host-network containers can leave their agent, operator, or
/// Envoy processes alive after their sandboxes stop. Retain exact source
/// container and Pod identities; signal only known Cilium daemons attached to
/// those identities so unrelated or destination processes remain untouched.
#[cfg(unix)]
pub fn stop_orphaned_cilium_processes(
    installation: &Installation,
    identity: &SourceCiliumIdentity,
) -> Result<usize> {
    if identity.container_ids.is_empty() && identity.pod_uids.is_empty() {
        cleanup_stale_cilium_envoy_sockets(installation)?;
        return Ok(0);
    }
    let proc_root = Path::new("/proc");
    let pids = processes_in_source_cilium_identity(proc_root, identity)?;
    if pids.is_empty() {
        eprintln!(
            "nodemigrate: found no remaining source Cilium daemon for {} source container ID(s) and {} source pod UID(s)",
            identity.container_ids.len(),
            identity.pod_uids.len()
        );
        cleanup_stale_cilium_envoy_sockets(installation)?;
        return Ok(0);
    }

    let stopped = signal_processes(&pids, libc::SIGTERM)?;
    if wait_for_source_process_exit(proc_root, identity, Duration::from_secs(5))? {
        eprintln!("nodemigrate: stopped {stopped} leftover source Cilium daemon process(es) by exact container or pod identity");
        cleanup_stale_cilium_envoy_sockets(installation)?;
        return Ok(stopped);
    }

    let remaining = processes_in_source_cilium_identity(proc_root, identity)?;
    let killed = signal_processes(&remaining, libc::SIGKILL)?;
    ensure!(
        wait_for_source_process_exit(proc_root, identity, Duration::from_secs(2))?,
        "source Cilium daemon process remained in a stopped source CRI container or pod after SIGKILL; refusing destination cutover"
    );
    eprintln!(
        "nodemigrate: stopped {stopped} leftover source Cilium daemon process(es) with SIGTERM and forced {killed} remaining source-identity process(es) to exit",
    );
    cleanup_stale_cilium_envoy_sockets(installation)?;
    Ok(stopped + killed)
}

#[cfg(not(unix))]
pub fn stop_orphaned_cilium_processes(
    installation: &Installation,
    _identity: &SourceCiliumIdentity,
) -> Result<usize> {
    cleanup_stale_cilium_envoy_sockets(installation)?;
    Ok(0)
}

fn cilium_host_container_ids(containers: &serde_json::Value) -> Vec<String> {
    let mut ids = containers
        .get("containers")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter(|container| {
            let name = container
                .get("metadata")
                .and_then(|metadata| metadata.get("name"))
                .and_then(serde_json::Value::as_str);
            let label_name = container
                .get("labels")
                .and_then(|labels| labels.get("io.kubernetes.container.name"))
                .or_else(|| {
                    container
                        .get("labels")
                        .and_then(|labels| labels.get("nodelet.dev/container-name"))
                })
                .and_then(serde_json::Value::as_str);
            matches!(
                name,
                Some("cilium-agent" | "cilium-envoy" | "cilium-operator")
            ) || matches!(
                label_name,
                Some("cilium-agent" | "cilium-envoy" | "cilium-operator")
            )
        })
        .filter_map(|container| {
            container
                .get("id")
                .and_then(serde_json::Value::as_str)
                .filter(|id| !id.is_empty())
                .map(str::to_string)
        })
        .collect::<Vec<_>>();
    ids.sort_unstable();
    ids.dedup();
    ids
}

#[cfg(unix)]
fn processes_in_source_cilium_identity(
    proc_root: &Path,
    identity: &SourceCiliumIdentity,
) -> Result<Vec<i32>> {
    let container_ids = identity
        .container_ids
        .iter()
        .map(String::as_str)
        .collect::<std::collections::HashSet<_>>();
    let pod_uids = identity
        .pod_uids
        .iter()
        .map(|uid| normalize_pod_uid(uid))
        .collect::<std::collections::HashSet<_>>();
    let entries = std::fs::read_dir(proc_root)
        .with_context(|| format!("listing processes in {}", proc_root.display()))?;
    let mut pids = Vec::new();
    for entry in entries {
        let entry = entry.with_context(|| format!("reading process in {}", proc_root.display()))?;
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<i32>().ok())
        else {
            continue;
        };
        let command_line = match std::fs::read(entry.path().join("cmdline")) {
            Ok(command_line) => command_line,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("reading command line for process {pid}"))
            }
        };
        if !is_cilium_host_process(&command_line) {
            continue;
        }
        let cgroup = match std::fs::read_to_string(entry.path().join("cgroup")) {
            Ok(cgroup) => cgroup,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(error).with_context(|| format!("reading cgroup for process {pid}"))
            }
        };
        if cgroup_container_id(&cgroup).is_some_and(|id| container_ids.contains(id))
            || cgroup_pod_uid(&cgroup).is_some_and(|uid| pod_uids.contains(&uid))
            || process_has_source_shim_ancestor(proc_root, pid, &container_ids)?
        {
            pids.push(pid);
        }
    }
    pids.sort_unstable();
    Ok(pids)
}

#[cfg(unix)]
fn process_has_source_shim_ancestor(
    proc_root: &Path,
    pid: i32,
    container_ids: &std::collections::HashSet<&str>,
) -> Result<bool> {
    let mut current = pid;
    for _ in 0..16 {
        let stat = match std::fs::read_to_string(proc_root.join(current.to_string()).join("stat")) {
            Ok(stat) => stat,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) => {
                return Err(error).with_context(|| format!("reading process stat for {current}"))
            }
        };
        let Some((_, fields)) = stat.rsplit_once(") ") else {
            return Ok(false);
        };
        let mut fields = fields.split_whitespace();
        let _state = fields.next();
        let Some(parent_pid) = fields.next().and_then(|value| value.parse::<i32>().ok()) else {
            return Ok(false);
        };
        if parent_pid <= 1 || parent_pid == current {
            return Ok(false);
        }
        let parent = proc_root.join(parent_pid.to_string());
        let command_line = match std::fs::read(parent.join("cmdline")) {
            Ok(command_line) => command_line,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("reading command line for process {parent_pid}"))
            }
        };
        if is_containerd_shim_for_source(&command_line, container_ids) {
            return Ok(true);
        }
        current = parent_pid;
    }
    Ok(false)
}

#[cfg(unix)]
fn is_containerd_shim_for_source(
    command_line: &[u8],
    container_ids: &std::collections::HashSet<&str>,
) -> bool {
    let args = command_line
        .split(|byte| *byte == 0)
        .filter_map(|arg| std::str::from_utf8(arg).ok())
        .collect::<Vec<_>>();
    let is_shim = args
        .first()
        .and_then(|executable| Path::new(executable).file_name())
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.starts_with("containerd-shim"));
    is_shim
        && args
            .windows(2)
            .any(|pair| pair[0] == "-id" && container_ids.contains(pair[1]))
}

#[cfg(unix)]
fn is_cilium_host_process(command_line: &[u8]) -> bool {
    command_line
        .split(|byte| *byte == 0)
        .next()
        .and_then(|executable| std::str::from_utf8(executable).ok())
        .and_then(|executable| Path::new(executable).file_name())
        .and_then(|name| name.to_str())
        .is_some_and(|name| {
            matches!(
                name,
                "cilium-agent"
                    | "cilium-operator"
                    | "cilium-operator-generic"
                    | "cilium-envoy"
                    | "cilium-envoy-starter"
            )
        })
}

#[cfg(unix)]
fn cgroup_container_id(cgroup: &str) -> Option<&str> {
    cgroup
        .lines()
        .filter_map(|line| line.rsplit(':').next())
        .find_map(|path| {
            path.split('/')
                .rev()
                .map(|component| {
                    let component = component.strip_suffix(".scope").unwrap_or(component);
                    component
                        .strip_prefix("cri-containerd-")
                        .unwrap_or(component)
                })
                .find(|component| {
                    component.len() == 64 && component.bytes().all(|byte| byte.is_ascii_hexdigit())
                })
        })
}

fn normalize_pod_uid(uid: &str) -> String {
    uid.replace('_', "-").to_ascii_lowercase()
}

#[cfg(unix)]
fn cgroup_pod_uid(cgroup: &str) -> Option<String> {
    cgroup
        .lines()
        .filter_map(|line| line.rsplit(':').next())
        .find_map(|path| {
            path.split('/').find_map(|component| {
                let component = component.strip_suffix(".slice").unwrap_or(component);
                let uid = component
                    .rsplit_once("-pod")
                    .map(|(_, uid)| uid)
                    .or_else(|| component.strip_prefix("pod"))?;
                let normalized = normalize_pod_uid(uid);
                let bytes = normalized.as_bytes();
                let valid_uuid = bytes.len() == 36
                    && [8, 13, 18, 23].iter().all(|index| bytes[*index] == b'-')
                    && bytes.iter().enumerate().all(|(index, byte)| {
                        [8, 13, 18, 23].contains(&index) || byte.is_ascii_hexdigit()
                    });
                valid_uuid.then_some(normalized)
            })
        })
}

#[cfg(unix)]
fn signal_processes(pids: &[i32], signal: i32) -> Result<usize> {
    let mut signalled = 0;
    for pid in pids {
        // SAFETY: `kill` has no pointer arguments. PIDs were read from `/proc`
        // immediately before this call; ESRCH is a harmless process-exit race.
        let result = unsafe { libc::kill(*pid, signal) };
        if result == 0 {
            signalled += 1;
        } else {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::ESRCH) {
                return Err(error).with_context(|| {
                    format!("sending signal {signal} to source Cilium daemon process {pid} in a source CRI container")
                });
            }
        }
    }
    Ok(signalled)
}

#[cfg(unix)]
fn wait_for_source_process_exit(
    proc_root: &Path,
    identity: &SourceCiliumIdentity,
    timeout: Duration,
) -> Result<bool> {
    let deadline = Instant::now() + timeout;
    loop {
        if processes_in_source_cilium_identity(proc_root, identity)?.is_empty() {
            return Ok(true);
        }
        if Instant::now() >= deadline {
            return Ok(false);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn source_pod_sandbox_ids(pods: &serde_json::Value) -> (Vec<String>, Vec<String>, Vec<String>) {
    let Some(items) = pods.get("items").and_then(serde_json::Value::as_array) else {
        return (Vec::new(), Vec::new(), Vec::new());
    };
    let mut sandboxes = Vec::new();
    let mut cilium_pod_uids = Vec::new();
    for pod in items {
        let Some(id) = pod
            .get("id")
            .and_then(serde_json::Value::as_str)
            .filter(|id| !id.is_empty())
        else {
            continue;
        };
        let is_cilium = pod
            .get("metadata")
            .and_then(|metadata| metadata.get("name"))
            .and_then(serde_json::Value::as_str)
            .is_some_and(|name| name.starts_with("cilium"))
            || pod
                .get("labels")
                .and_then(|labels| labels.get("k8s-app"))
                .and_then(serde_json::Value::as_str)
                .is_some_and(|app| app == "cilium" || app == "cilium-envoy");
        let is_ready =
            pod.get("state").and_then(serde_json::Value::as_str) == Some("SANDBOX_READY");
        if is_cilium {
            if let Some(uid) = pod
                .get("metadata")
                .and_then(|metadata| metadata.get("uid"))
                .and_then(serde_json::Value::as_str)
                .or_else(|| {
                    pod.get("labels")
                        .and_then(|labels| labels.get("io.kubernetes.pod.uid"))
                        .and_then(serde_json::Value::as_str)
                })
                .or_else(|| {
                    pod.get("labels")
                        .and_then(|labels| labels.get("nodelet.dev/pod-uid"))
                        .and_then(serde_json::Value::as_str)
                })
                .filter(|uid| !uid.is_empty())
            {
                cilium_pod_uids.push(uid.to_string());
            }
        }
        sandboxes.push((id.to_string(), is_ready, is_cilium));
    }
    // CNI teardown for ordinary pods may depend on the Cilium agent. Stop and
    // remove Cilium pods last, after the rest of the source sandboxes.
    sandboxes.sort_by_key(|(_, _, is_cilium)| *is_cilium);
    let ready = sandboxes
        .iter()
        .filter(|(_, is_ready, _)| *is_ready)
        .map(|(id, _, _)| id.clone())
        .collect();
    let all = sandboxes.into_iter().map(|(id, _, _)| id).collect();
    cilium_pod_uids.sort_unstable();
    cilium_pod_uids.dedup();
    (ready, all, cilium_pod_uids)
}

fn running_sandbox_ids(containers: &serde_json::Value) -> std::collections::HashSet<String> {
    containers
        .get("containers")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter(|container| {
            container.get("state").and_then(serde_json::Value::as_str) == Some("CONTAINER_RUNNING")
        })
        .filter_map(|container| {
            container
                .get("podSandboxId")
                .and_then(serde_json::Value::as_str)
                .filter(|id| !id.is_empty())
                .map(str::to_string)
        })
        .collect()
}

fn ensure_failed_stops_are_inactive(
    failures: &[(String, String)],
    running: &std::collections::HashSet<String>,
) -> Result<()> {
    for (id, error) in failures {
        ensure!(
            !running.contains(id),
            "stopping source pod sandbox {id} failed while its container is still running: {error}"
        );
    }
    Ok(())
}

fn cleanup_stale_cilium_envoy_sockets(installation: &Installation) -> Result<usize> {
    let is_cilium = installation
        .cluster
        .as_ref()
        .and_then(|cluster| cluster.cni.as_deref())
        .is_some_and(|cni| cni.eq_ignore_ascii_case("cilium"));
    if !is_cilium {
        return Ok(0);
    }
    #[cfg(unix)]
    {
        let removed = remove_unix_sockets(Path::new("/var/run/cilium/envoy/sockets"))
            .context("removing stale Cilium Envoy sockets after source containers stopped")?;
        eprintln!("nodemigrate: removed {removed} stale Cilium Envoy Unix socket(s)");
        Ok(removed)
    }
    #[cfg(not(unix))]
    {
        Ok(0)
    }
}

#[cfg(unix)]
fn remove_unix_sockets(directory: &Path) -> Result<usize> {
    let metadata = match std::fs::symlink_metadata(directory) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(error) => {
            return Err(error).with_context(|| format!("reading {}", directory.display()))
        }
    };
    ensure!(
        metadata.file_type().is_dir(),
        "Cilium Envoy socket path {} is not a directory",
        directory.display()
    );
    let mut removed = 0;
    for entry in
        std::fs::read_dir(directory).with_context(|| format!("listing {}", directory.display()))?
    {
        let entry = entry.with_context(|| format!("reading entry in {}", directory.display()))?;
        let path = entry.path();
        let metadata = std::fs::symlink_metadata(&path)
            .with_context(|| format!("reading socket metadata for {}", path.display()))?;
        if metadata.file_type().is_socket() {
            std::fs::remove_file(&path)
                .with_context(|| format!("removing stale Unix socket {}", path.display()))?;
            removed += 1;
        }
    }
    Ok(removed)
}

fn wait_for_api_port_release() -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        match std::net::TcpListener::bind(("0.0.0.0", 6443)) {
            Ok(listener) => {
                drop(listener);
                return Ok(());
            }
            Err(error) if error.kind() == std::io::ErrorKind::AddrInUse => {}
            Err(error) => return Err(error).context("checking whether API port 6443 is free"),
        }
        ensure!(
            Instant::now() < deadline,
            "source Kubernetes API still occupies port 6443 after stopping its static pods"
        );
        std::thread::sleep(Duration::from_millis(250));
    }
}

fn static_pod_sandbox_ids(pods: &serde_json::Value) -> Vec<String> {
    pods.get("items")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter(|pod| {
            let namespace = pod
                .pointer("/metadata/namespace")
                .and_then(serde_json::Value::as_str);
            let static_source = pod
                .pointer("/labels/kubernetes.io~1config.source")
                .and_then(serde_json::Value::as_str)
                == Some("file");
            let control_plane_name = pod
                .pointer("/metadata/name")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|name| {
                    [
                        "kube-apiserver-",
                        "etcd-",
                        "kube-scheduler-",
                        "kube-controller-manager-",
                    ]
                    .iter()
                    .any(|prefix| name.starts_with(prefix))
                });
            namespace == Some("kube-system") && (static_source || control_plane_name)
        })
        .filter_map(|pod| {
            pod.pointer("/id")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        })
        .collect()
}

pub fn restore(installation: &Installation, previous: PreviousServiceState) -> Result<()> {
    let manager = installation
        .service_manager
        .context("source service manager is unknown")?;
    let name = &installation.service_name;
    if !previous.services.is_empty() {
        restore_nodestore_stack(manager, &previous)?;
        return Ok(());
    }
    match manager {
        ServiceManager::Systemd => {
            if previous.enabled {
                checked("systemctl", &["enable", &format!("{name}.service")])?;
            }
            if previous.active {
                checked("systemctl", &["start", &format!("{name}.service")])?;
            }
        }
        ServiceManager::OpenRc => {
            if previous.enabled {
                checked("rc-update", &["add", name, "default"])?;
            }
            if previous.active {
                checked("rc-service", &[name, "start"])?;
            }
        }
        ServiceManager::SysVInit => {
            if previous.enabled {
                set_sysv_enabled(name, true)?;
            }
            if previous.active {
                checked("service", &[name, "start"])?;
            }
        }
        ServiceManager::Runit => {
            if let Some(dir) = runit_service_dir(name) {
                let down = dir.join("down");
                if previous.enabled {
                    let _ = std::fs::remove_file(down);
                }
                if previous.active {
                    checked("sv", &["up", &dir.to_string_lossy()])?;
                }
            }
        }
    }
    Ok(())
}

/// Stop any partially started nodestore stack before restoring a failed
/// forward migration's source distribution. An empty stack is expected when
/// bootstrap failed before installing units.
pub fn stop_nodestore_for_rollback(installation: &Installation) -> Result<()> {
    let manager = installation
        .service_manager
        .context("service manager is unknown; cannot stop the partial nodestore stack")?;
    stop_nodestore_stack(manager, false).map(|_| ())
}

fn stop_nodestore_stack(
    manager: ServiceManager,
    require_active: bool,
) -> Result<PreviousServiceState> {
    let services = [
        "nodelet",
        "nodeproxy",
        "nodecontroller",
        "nodescheduler",
        "flanneld",
        "kube-apiserver",
        "nodeapiserver",
        "nodestore",
    ]
    .into_iter()
    .filter(|name| service_exists(manager, name))
    .map(|name| ServiceState {
        name: name.to_string(),
        enabled: service_enabled(manager, name),
        active: service_active(manager, name),
    })
    .collect::<Vec<_>>();
    ensure!(
        !require_active || services.iter().any(|service| service.active),
        "no active nodebootstrap services were found to stop"
    );
    let previous = PreviousServiceState {
        enabled: false,
        active: false,
        services,
    };
    for service in &previous.services {
        if service.active || service.enabled {
            let result = if service.active {
                stop_and_disable(manager, &service.name)
            } else {
                disable_named(manager, &service.name)
            };
            if let Err(error) = result {
                if let Err(restore_error) = restore_nodestore_stack(manager, &previous) {
                    bail!("stopping nodebootstrap service '{}' failed ({error:#}) and restoring the stack failed ({restore_error:#})", service.name);
                }
                return Err(error)
                    .with_context(|| format!("stopping nodebootstrap service {}", service.name));
            }
        }
    }
    Ok(previous)
}

fn restore_nodestore_stack(manager: ServiceManager, previous: &PreviousServiceState) -> Result<()> {
    // Restart dependencies in the opposite order from shutdown: datastore,
    // apiserver and network first, then controllers and node services.
    for service in previous.services.iter().rev() {
        restore_named(manager, service)?;
    }
    Ok(())
}

pub fn activate(installation: &Installation) -> Result<()> {
    let manager = installation
        .service_manager
        .context("target service manager could not be identified")?;
    ensure!(
        installation.distribution != crate::request::Distribution::Nodestore,
        "activating an existing nodestore target requires restoring its full stack"
    );
    restore_named(
        manager,
        &ServiceState {
            name: installation.service_name.clone(),
            enabled: true,
            active: true,
        },
    )
}

fn restore_named(manager: ServiceManager, service: &ServiceState) -> Result<()> {
    let name = service.name.as_str();
    match manager {
        ServiceManager::Systemd => {
            if service.enabled {
                checked("systemctl", &["enable", &format!("{name}.service")])?;
            }
            if service.active {
                checked("systemctl", &["start", &format!("{name}.service")])?;
            }
        }
        ServiceManager::OpenRc => {
            if service.enabled {
                checked("rc-update", &["add", name, "default"])?;
            }
            if service.active {
                checked("rc-service", &[name, "start"])?;
            }
        }
        ServiceManager::SysVInit => {
            if service.enabled {
                set_sysv_enabled(name, true)?;
            }
            if service.active {
                checked("service", &[name, "start"])?;
            }
        }
        ServiceManager::Runit => {
            let dir = runit_service_dir(name)
                .with_context(|| format!("could not find runit service directory for {name}"))?;
            if service.enabled {
                let _ = std::fs::remove_file(dir.join("down"));
            }
            if service.active {
                checked("sv", &["up", &dir.to_string_lossy()])?;
            }
        }
    }
    Ok(())
}

pub fn uninstall_k3s() -> Result<()> {
    checked("/usr/local/bin/k3s-uninstall.sh", &[])
}

pub fn uninstall_source(installation: &Installation) -> Result<()> {
    match installation.distribution {
        crate::request::Distribution::K3s => uninstall_k3s(),
        crate::request::Distribution::Kubernetes => {
            let kubeadm = installation
                .binary
                .as_ref()
                .context("detected Kubernetes installation has no kubeadm binary")?;
            checked(&kubeadm.to_string_lossy(), &["reset", "--force"])
        }
        crate::request::Distribution::Nodestore => {
            let (binary, combined) =
                find_nodebootstrap().context("nodebootstrap or notk8s binary was not found")?;
            if combined {
                checked(&binary.to_string_lossy(), &["bootstrap", "--uninstall"])
            } else {
                checked(&binary.to_string_lossy(), &["--uninstall"])
            }
        }
    }
}

fn find_nodebootstrap() -> Option<(std::path::PathBuf, bool)> {
    let mut directories: Vec<std::path::PathBuf> = std::env::var_os("PATH")
        .map(|path| std::env::split_paths(&path).collect())
        .unwrap_or_default();
    if let Ok(current_exe) = std::env::current_exe() {
        if let Some(parent) = current_exe.parent() {
            directories.push(parent.to_path_buf());
        }
    }
    directories.extend(["/usr/local/bin", "/usr/bin"].map(std::path::PathBuf::from));
    for directory in directories {
        for (name, combined) in [("notk8s", true), ("nodebootstrap", false)] {
            let path = directory.join(name);
            if path.is_file() {
                return Some((path, combined));
            }
        }
    }
    None
}

fn stop_and_disable(manager: ServiceManager, name: &str) -> Result<()> {
    match manager {
        ServiceManager::Systemd => checked(
            "systemctl",
            &["disable", "--now", &format!("{name}.service")],
        ),
        ServiceManager::OpenRc => {
            checked("rc-service", &[name, "stop"])?;
            checked("rc-update", &["del", name, "default"]).or_else(|error| {
                // OpenRC returns nonzero when the service was not in the
                // requested runlevel; stop is still required and succeeded.
                if !std::path::Path::new("/etc/runlevels/default")
                    .join(name)
                    .exists()
                {
                    Ok(())
                } else {
                    Err(error)
                }
            })
        }
        ServiceManager::SysVInit => {
            checked("service", &[name, "stop"])?;
            set_sysv_enabled(name, false)
        }
        ServiceManager::Runit => {
            let dir = runit_service_dir(name)
                .with_context(|| format!("could not find runit service directory for {name}"))?;
            checked("sv", &["down", &dir.to_string_lossy()])?;
            std::fs::write(dir.join("down"), b"")
                .with_context(|| format!("disabling runit service {name}"))
        }
    }
}

fn runit_service_dir(name: &str) -> Option<std::path::PathBuf> {
    ["/etc/service", "/var/service"]
        .into_iter()
        .map(|root| std::path::Path::new(root).join(name))
        .find(|path| path.exists())
}

fn service_exists(manager: ServiceManager, name: &str) -> bool {
    match manager {
        ServiceManager::Systemd => [
            format!("/etc/systemd/system/{name}.service"),
            format!("/usr/lib/systemd/system/{name}.service"),
            format!("/lib/systemd/system/{name}.service"),
        ]
        .iter()
        .any(|path| std::path::Path::new(path).exists()),
        ServiceManager::OpenRc | ServiceManager::SysVInit => {
            std::path::Path::new("/etc/init.d").join(name).is_file()
        }
        ServiceManager::Runit => runit_service_dir(name).is_some(),
    }
}

fn service_enabled(manager: ServiceManager, name: &str) -> bool {
    match manager {
        ServiceManager::Systemd => {
            command("systemctl", &["is-enabled", &format!("{name}.service")])
                .is_ok_and(|output| output.status.success())
        }
        ServiceManager::OpenRc => std::path::Path::new("/etc/runlevels/default")
            .join(name)
            .exists(),
        ServiceManager::SysVInit => sysv_enabled(name),
        ServiceManager::Runit => {
            runit_service_dir(name).is_some_and(|dir| !dir.join("down").exists())
        }
    }
}

fn service_active(manager: ServiceManager, name: &str) -> bool {
    match manager {
        ServiceManager::Systemd => command("systemctl", &["is-active", &format!("{name}.service")])
            .is_ok_and(|output| output.status.success()),
        ServiceManager::OpenRc => {
            command("rc-service", &[name, "status"]).is_ok_and(|output| output.status.success())
        }
        ServiceManager::SysVInit => {
            command("service", &[name, "status"]).is_ok_and(|output| output.status.success())
        }
        ServiceManager::Runit => runit_service_dir(name).is_some_and(|dir| {
            command("sv", &["status", &dir.to_string_lossy()]).is_ok_and(|output| {
                output.status.success()
                    && String::from_utf8_lossy(&output.stdout).starts_with("run:")
            })
        }),
    }
}

fn disable_named(manager: ServiceManager, name: &str) -> Result<()> {
    match manager {
        ServiceManager::Systemd => checked("systemctl", &["disable", &format!("{name}.service")]),
        ServiceManager::OpenRc => {
            checked("rc-update", &["del", name, "default"]).or_else(|error| {
                if !std::path::Path::new("/etc/runlevels/default")
                    .join(name)
                    .exists()
                {
                    Ok(())
                } else {
                    Err(error)
                }
            })
        }
        ServiceManager::SysVInit => set_sysv_enabled(name, false),
        ServiceManager::Runit => {
            let dir = runit_service_dir(name)
                .with_context(|| format!("could not find runit service directory for {name}"))?;
            std::fs::write(dir.join("down"), b"")
                .with_context(|| format!("disabling runit service {name}"))
        }
    }
}

fn sysv_enabled(name: &str) -> bool {
    let Ok(runlevels) = std::fs::read_dir("/etc") else {
        return false;
    };
    runlevels.flatten().any(|entry| {
        let name_matches =
            entry.file_name().to_string_lossy().starts_with("rc") && entry.path().is_dir();
        name_matches
            && std::fs::read_dir(entry.path()).is_ok_and(|entries| {
                entries.flatten().any(|service| {
                    service.file_name().to_string_lossy().starts_with('S')
                        && service.file_name().to_string_lossy().ends_with(name)
                })
            })
    })
}

fn set_sysv_enabled(name: &str, enabled: bool) -> Result<()> {
    if command("chkconfig", &[name, if enabled { "on" } else { "off" }])
        .is_ok_and(|output| output.status.success())
    {
        return Ok(());
    }
    let action = if enabled { "defaults" } else { "disable" };
    checked("update-rc.d", &[name, action])
}

fn checked(program: &str, args: &[&str]) -> Result<()> {
    let output = command(program, args)?;
    ensure!(
        output.status.success(),
        "{program} {} failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(())
}

fn command(program: &str, args: &[&str]) -> Result<Output> {
    Command::new(program)
        .args(args)
        .output()
        .with_context(|| format!("running {program}"))
}

#[cfg(test)]
mod tests {
    use super::{
        cilium_host_container_ids, ensure_failed_stops_are_inactive, running_sandbox_ids,
        source_pod_sandbox_ids, static_pod_sandbox_ids, SourceCiliumIdentity,
    };

    const SOURCE_CONTAINER_ID: &str =
        "ef96e5cf937fed840c1bfcc03df0ef667927c7f666ba4963da35faaa9f80f39a";

    #[test]
    fn selects_only_kube_system_file_static_pod_sandboxes() {
        let pods = serde_json::json!({
            "items": [
                {
                    "id": "static-sandbox",
                    "metadata": {"name": "custom-static-pod", "namespace": "kube-system"},
                    "labels": {"kubernetes.io/config.source": "file"}
                },
                {
                    "id": "apiserver-sandbox",
                    "metadata": {"name": "kube-apiserver-node-a", "namespace": "kube-system"},
                    "labels": {}
                },
                {
                    "id": "controller-sandbox",
                    "metadata": {"namespace": "kube-system"},
                    "labels": {"kubernetes.io/config.source": "api"}
                },
                {
                    "id": "application-sandbox",
                    "metadata": {"namespace": "apps"},
                    "labels": {"kubernetes.io/config.source": "file"}
                }
            ]
        });
        assert_eq!(
            static_pod_sandbox_ids(&pods),
            ["static-sandbox", "apiserver-sandbox"]
        );
    }

    #[test]
    fn selects_every_source_sandbox_and_only_stops_ready_ones() {
        let pods = serde_json::json!({
            "items": [
                {"id": "cilium", "state": "SANDBOX_READY", "metadata": {"name": "cilium-agent", "namespace": "kube-system", "uid": "11111111-2222-4333-8444-555555555555"}},
                {"id": "ready", "state": "SANDBOX_READY"},
                {"id": "not-ready", "state": "SANDBOX_NOTREADY"},
                {"id": "other-cilium", "state": "SANDBOX_READY", "metadata": {"name": "cilium-envoy-node-a", "namespace": "kube-system", "uid": "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee"}},
                {"id": "operator", "state": "SANDBOX_READY", "metadata": {"name": "cilium-operator-abc", "namespace": "kube-system", "uid": "22222222-3333-4444-8555-666666666666"}},
                {"metadata": {"name": "missing-id"}, "state": "SANDBOX_READY"},
                {"id": "", "state": "SANDBOX_READY"}
            ]
        });

        assert_eq!(
            source_pod_sandbox_ids(&pods),
            (
                vec![
                    "ready".to_string(),
                    "cilium".to_string(),
                    "other-cilium".to_string(),
                    "operator".to_string()
                ],
                vec![
                    "ready".to_string(),
                    "not-ready".to_string(),
                    "cilium".to_string(),
                    "other-cilium".to_string(),
                    "operator".to_string()
                ],
                vec![
                    "11111111-2222-4333-8444-555555555555".to_string(),
                    "22222222-3333-4444-8555-666666666666".to_string(),
                    "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee".to_string()
                ]
            )
        );
    }

    #[test]
    fn source_sandbox_listing_without_items_is_empty() {
        assert_eq!(
            source_pod_sandbox_ids(&serde_json::json!({})),
            (vec![], vec![], vec![])
        );
    }

    #[test]
    fn running_container_ids_are_grouped_by_pod_sandbox() {
        let containers = serde_json::json!({
            "containers": [
                {"podSandboxId": "running", "state": "CONTAINER_RUNNING"},
                {"podSandboxId": "exited", "state": "CONTAINER_EXITED"},
                {"podSandboxId": "", "state": "CONTAINER_RUNNING"}
            ]
        });

        assert_eq!(
            running_sandbox_ids(&containers),
            ["running".to_string()].into_iter().collect()
        );
    }

    #[test]
    fn tolerates_stop_failure_only_when_cri_confirms_no_running_container() {
        let failures = vec![("stopped".to_string(), "deadline exceeded".to_string())];
        assert!(
            ensure_failed_stops_are_inactive(&failures, &std::collections::HashSet::new()).is_ok()
        );

        let running = ["stopped".to_string()].into_iter().collect();
        let error = ensure_failed_stops_are_inactive(&failures, &running).unwrap_err();
        assert!(error.to_string().contains("still running"));
        assert!(error.to_string().contains("deadline exceeded"));
    }

    #[test]
    fn finds_cilium_host_daemon_cri_containers_by_name_or_label() {
        let containers = serde_json::json!({
            "containers": [
                {"id": SOURCE_CONTAINER_ID, "metadata": {"name": "cilium-envoy"}},
                {"id": "container-1", "metadata": {"name": "cilium-agent"}},
                {"id": "container-2", "labels": {"io.kubernetes.container.name": "cilium-envoy"}},
                {"id": "container-3", "labels": {"nodelet.dev/container-name": "cilium-envoy"}},
                {"id": "container-4", "metadata": {"name": "cilium-operator"}},
                {"id": "ordinary", "metadata": {"name": "ordinary-app"}},
                {"id": "", "metadata": {"name": "cilium-envoy"}},
                {"id": "container-2", "metadata": {"name": "cilium-envoy"}}
            ]
        });

        assert_eq!(
            cilium_host_container_ids(&containers),
            vec![
                "container-1".to_string(),
                "container-2".to_string(),
                "container-3".to_string(),
                "container-4".to_string(),
                SOURCE_CONTAINER_ID.to_string()
            ]
        );
    }

    #[cfg(unix)]
    #[test]
    fn parses_container_ids_from_systemd_and_cgroupfs_paths() {
        use super::cgroup_container_id;

        let systemd = format!(
            "0::/kubepods.slice/kubepods-besteffort.slice/cri-containerd-{SOURCE_CONTAINER_ID}.scope\n"
        );
        let cgroupfs = format!("0::/kubepods/besteffort/{SOURCE_CONTAINER_ID}\n");

        assert_eq!(cgroup_container_id(&systemd), Some(SOURCE_CONTAINER_ID));
        assert_eq!(cgroup_container_id(&cgroupfs), Some(SOURCE_CONTAINER_ID));
        assert_eq!(cgroup_container_id("0::/system.slice/k3s.service\n"), None);
        assert_eq!(
            cgroup_container_id("0::/kubepods/cri-containerd-not-a-container.scope\n"),
            None
        );
    }

    #[cfg(unix)]
    #[test]
    fn parses_pod_uids_from_systemd_and_cgroupfs_paths() {
        use super::cgroup_pod_uid;

        let pod_uid = "8536f215-fc22-41fa-b8b6-f0245c88e125";
        assert_eq!(
            cgroup_pod_uid(&format!(
                "0::/kubepods.slice/kubepods-besteffort.slice/kubepods-besteffort-pod{}.slice/cri-containerd-{SOURCE_CONTAINER_ID}.scope\n",
                pod_uid.replace('-', "_")
            )),
            Some(pod_uid.to_string())
        );
        assert_eq!(
            cgroup_pod_uid(&format!("0::/kubepods/besteffort/pod{pod_uid}/container\n")),
            Some(pod_uid.to_string())
        );
        assert_eq!(cgroup_pod_uid("0::/system.slice/k3s.service\n"), None);
        assert_eq!(
            cgroup_pod_uid("0::/kubepods.slice/kubepods-besteffort-podnot-a-uid.slice\n"),
            None
        );
    }

    #[cfg(unix)]
    #[test]
    fn matches_known_cilium_daemons_only_in_recorded_source_containers_or_pods() {
        use super::processes_in_source_cilium_identity;

        let proc_root = tempfile::tempdir().unwrap();
        let source_process = proc_root.path().join("101");
        std::fs::create_dir(&source_process).unwrap();
        std::fs::write(
            source_process.join("cmdline"),
            b"/usr/bin/cilium-envoy\0--config\0",
        )
        .unwrap();
        std::fs::write(
            source_process.join("cgroup"),
            format!("0::/kubepods/cri-containerd-{SOURCE_CONTAINER_ID}.scope\n"),
        )
        .unwrap();

        let other_process = proc_root.path().join("102");
        std::fs::create_dir(&other_process).unwrap();
        std::fs::write(other_process.join("cmdline"), b"/usr/bin/cilium-envoy\0").unwrap();
        std::fs::write(
            other_process.join("cgroup"),
            "0::/kubepods/cri-containerd-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa.scope\n",
        )
        .unwrap();

        let source_agent = proc_root.path().join("108");
        std::fs::create_dir(&source_agent).unwrap();
        std::fs::write(source_agent.join("cmdline"), b"/usr/bin/cilium-agent\0").unwrap();
        std::fs::write(
            source_agent.join("cgroup"),
            format!("0::/kubepods/cri-containerd-{SOURCE_CONTAINER_ID}.scope\n"),
        )
        .unwrap();

        let source_operator = proc_root.path().join("109");
        std::fs::create_dir(&source_operator).unwrap();
        std::fs::write(
            source_operator.join("cmdline"),
            b"/usr/bin/cilium-operator-generic\0",
        )
        .unwrap();
        std::fs::write(
            source_operator.join("cgroup"),
            "0::/kubepods.slice/kubepods-besteffort.slice/kubepods-besteffort-pod8536f215_fc22_41fa_b8b6_f0245c88e125.slice/cri-containerd-bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb.scope\n",
        )
        .unwrap();

        let other_agent = proc_root.path().join("110");
        std::fs::create_dir(&other_agent).unwrap();
        std::fs::write(other_agent.join("cmdline"), b"/usr/bin/cilium-agent\0").unwrap();
        std::fs::write(
            other_agent.join("cgroup"),
            "0::/kubepods.slice/kubepods-besteffort.slice/kubepods-besteffort-podaaaaaaaa_bbbb_4ccc_8ddd_eeeeeeeeeeee.slice/cri-containerd-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa.scope\n",
        )
        .unwrap();

        let same_container_other_program = proc_root.path().join("103");
        std::fs::create_dir(&same_container_other_program).unwrap();
        std::fs::write(
            same_container_other_program.join("cmdline"),
            b"/usr/bin/containerd-shim\0",
        )
        .unwrap();
        std::fs::write(
            same_container_other_program.join("cgroup"),
            format!("0::/kubepods/cri-containerd-{SOURCE_CONTAINER_ID}.scope\n"),
        )
        .unwrap();

        let envoy_with_shim_identity = proc_root.path().join("104");
        std::fs::create_dir(&envoy_with_shim_identity).unwrap();
        std::fs::write(
            envoy_with_shim_identity.join("cmdline"),
            b"/usr/bin/cilium-envoy-starter\0",
        )
        .unwrap();
        std::fs::write(
            envoy_with_shim_identity.join("cgroup"),
            "0::/system.slice/k3s.service\n",
        )
        .unwrap();
        std::fs::write(
            envoy_with_shim_identity.join("stat"),
            "104 (cilium-envoy-starter) S 105 1 1 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0\n",
        )
        .unwrap();

        let source_shim = proc_root.path().join("105");
        std::fs::create_dir(&source_shim).unwrap();
        std::fs::write(
            source_shim.join("cmdline"),
            format!(
                "/usr/bin/containerd-shim-runc-v2\0-namespace\0k8s.io\0-id\0{SOURCE_CONTAINER_ID}\0-address\0/run/k3s/containerd/containerd.sock\0"
            ),
        )
        .unwrap();
        std::fs::write(source_shim.join("cgroup"), "0::/system.slice/k3s.service\n").unwrap();
        std::fs::write(
            source_shim.join("stat"),
            "105 (containerd-shim) S 1 1 1 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0\n",
        )
        .unwrap();

        let source_pod_envoy = proc_root.path().join("106");
        std::fs::create_dir(&source_pod_envoy).unwrap();
        std::fs::write(
            source_pod_envoy.join("cmdline"),
            b"/usr/bin/cilium-envoy\0--config\0",
        )
        .unwrap();
        std::fs::write(
            source_pod_envoy.join("cgroup"),
            "0::/kubepods.slice/kubepods-besteffort.slice/kubepods-besteffort-pod8536f215_fc22_41fa_b8b6_f0245c88e125.slice/cri-containerd-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa.scope\n",
        )
        .unwrap();

        let other_pod_envoy = proc_root.path().join("107");
        std::fs::create_dir(&other_pod_envoy).unwrap();
        std::fs::write(
            other_pod_envoy.join("cmdline"),
            b"/usr/bin/cilium-envoy\0--config\0",
        )
        .unwrap();
        std::fs::write(
            other_pod_envoy.join("cgroup"),
            "0::/kubepods.slice/kubepods-besteffort.slice/kubepods-besteffort-podaaaaaaaa_bbbb_4ccc_8ddd_eeeeeeeeeeee.slice/cri-containerd-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa.scope\n",
        )
        .unwrap();

        assert_eq!(
            processes_in_source_cilium_identity(
                proc_root.path(),
                &SourceCiliumIdentity {
                    container_ids: vec![SOURCE_CONTAINER_ID.to_string()],
                    pod_uids: vec!["8536f215-fc22-41fa-b8b6-f0245c88e125".to_string()],
                }
            )
            .unwrap(),
            vec![101, 104, 106, 108, 109]
        );
    }

    #[cfg(unix)]
    #[test]
    fn removes_only_unix_sockets_from_cilium_socket_directory() {
        use super::remove_unix_sockets;
        use std::{os::unix::net::UnixListener, path::Path};

        let temp = tempfile::tempdir().unwrap();
        let socket_path = temp.path().join("envoy.sock");
        let listener = UnixListener::bind(&socket_path).unwrap();
        let regular_file = temp.path().join("keep.txt");
        std::fs::write(&regular_file, b"keep").unwrap();
        drop(listener);

        assert_eq!(remove_unix_sockets(temp.path()).unwrap(), 1);
        assert!(!Path::new(&socket_path).exists());
        assert_eq!(std::fs::read(regular_file).unwrap(), b"keep");
    }
}
