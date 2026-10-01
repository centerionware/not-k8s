use std::process::{Command, Output};
use std::time::{Duration, Instant};
#[cfg(unix)]
use std::{os::unix::fs::FileTypeExt, path::Path};

use anyhow::{Context, Result, bail, ensure};

use crate::{
    detect::{Installation, NodeRole, ServiceManager},
    request::Distribution,
};

#[derive(Debug, Clone)]
pub struct PreviousServiceState {
    enabled: bool,
    active: bool,
    services: Vec<ServiceState>,
    runtime: Option<ServiceState>,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct SourceCiliumIdentity {
    sandbox_ids: Vec<String>,
    container_ids: Vec<String>,
    pod_uids: Vec<String>,
}

// cri-tools defaults CRI calls to a two second deadline. Removing a stopped
// sandbox can take longer while the runtime tears down its network namespace.
const CRICTL_CLEANUP_TIMEOUT: &str = "60s";
const CRICTL_CLEANUP_TRANSITION_ATTEMPTS: usize = 16;
const CRICTL_CLEANUP_DEADLINE_ATTEMPTS: usize = 2;

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

fn runtime_service_state(
    manager: ServiceManager,
    installation: &Installation,
) -> Result<Option<ServiceState>> {
    if installation.distribution == Distribution::K3s {
        return Ok(None);
    }
    let configured_name = std::env::var("NODEMIGRATE_RUNTIME_SERVICE")
        .ok()
        .filter(|name| !name.trim().is_empty())
        .map(|name| name.trim().to_string());
    let name = configured_name
        .or_else(|| runtime_service_name(installation.runtime_endpoint.as_deref()))
        .context("could not identify the source CRI service; set NODEMIGRATE_RUNTIME_SERVICE to its service name")?;
    if !service_exists(manager, &name) {
        if installation.distribution == Distribution::Nodestore
            && installation.runtime_endpoint.is_none()
            && std::env::var_os("NODEMIGRATE_RUNTIME_SERVICE").is_none()
        {
            return Ok(None);
        }
        anyhow::bail!(
            "source CRI service '{name}' was not found; set NODEMIGRATE_RUNTIME_SERVICE to the installed runtime service"
        );
    }
    let active = service_active(manager, &name);
    if installation.distribution == Distribution::Nodestore && !active {
        return Ok(None);
    }
    ensure!(
        active,
        "source CRI service '{name}' is not active; refusing to migrate while runtime state is uncertain"
    );
    Ok(Some(ServiceState {
        name: name.clone(),
        enabled: service_enabled(manager, &name),
        active,
    }))
}

fn runtime_service_name(endpoint: Option<&str>) -> Option<String> {
    let endpoint = endpoint.unwrap_or("");
    if endpoint.contains("crio") {
        Some("crio".to_string())
    } else if endpoint.contains("cri-dockerd") {
        Some("docker".to_string())
    } else if endpoint.is_empty() || endpoint.contains("containerd") {
        Some("containerd".to_string())
    } else {
        None
    }
}

fn installation_runtime_endpoint(installation: &Installation) -> String {
    installation
        .runtime_endpoint
        .clone()
        .unwrap_or_else(|| "unix:///run/containerd/containerd.sock".to_string())
}

fn installation_runtime_service_name(installation: &Installation) -> Result<String> {
    std::env::var("NODEMIGRATE_RUNTIME_SERVICE")
        .ok()
        .filter(|name| !name.trim().is_empty())
        .map(|name| name.trim().to_string())
        .or_else(|| runtime_service_name(installation.runtime_endpoint.as_deref()))
        .context("could not identify the retained Kubernetes CRI service")
}

fn runtime_endpoint_available(endpoint: &str) -> bool {
    endpoint
        .strip_prefix("unix://")
        .map_or(true, |path| std::path::Path::new(path).exists())
}

fn capture_source_cilium_identity(installation: &Installation) -> Result<SourceCiliumIdentity> {
    let endpoint = std::env::var("NODEMIGRATE_CRI_ENDPOINT")
        .ok()
        .filter(|value| !value.is_empty())
        .or_else(|| installation.runtime_endpoint.clone())
        .unwrap_or_else(|| "unix:///run/containerd/containerd.sock".to_string());
    capture_cilium_identity_at(installation, &endpoint)
}

fn capture_nodelet_sandbox_ids(installation: &Installation) -> Result<Vec<String>> {
    let endpoint = std::env::var("NODEMIGRATE_CRI_ENDPOINT")
        .ok()
        .filter(|value| !value.is_empty())
        .or_else(|| installation.runtime_endpoint.clone())
        .unwrap_or_else(|| "unix:///run/containerd/containerd.sock".to_string());
    let pods = checked_output(
        "crictl",
        &["--runtime-endpoint", &endpoint, "pods", "-o", "json"],
        "listing nodelet-managed source pod sandboxes",
    )?;
    let pods: serde_json::Value = serde_json::from_slice(&pods)
        .context("parsing nodelet-managed source pod sandboxes")?;
    Ok(nodelet_source_sandbox_ids(&pods))
}

fn nodelet_source_sandbox_ids(pods: &serde_json::Value) -> Vec<String> {
    pods.get("items")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter(|pod| {
            pod.get("labels")
                .and_then(|labels| labels.get("nodelet.dev/pod-uid"))
                .and_then(serde_json::Value::as_str)
                .is_some_and(|uid| !uid.is_empty())
        })
        .filter_map(|pod| {
            pod.get("id")
                .and_then(serde_json::Value::as_str)
                .filter(|id| !id.is_empty())
                .map(str::to_owned)
        })
        .collect()
}

fn stop_nodelet_source_sandboxes(
    installation: &Installation,
    sandbox_ids: &[String],
) -> Result<()> {
    let endpoint = std::env::var("NODEMIGRATE_CRI_ENDPOINT")
        .ok()
        .filter(|value| !value.is_empty())
        .or_else(|| installation.runtime_endpoint.clone())
        .unwrap_or_else(|| "unix:///run/containerd/containerd.sock".to_string());
    for id in sandbox_ids {
        checked_cri_cleanup(&endpoint, "stopp", id)
            .with_context(|| format!("stopping nodelet-managed pod sandbox {id}"))?;
        checked_cri_cleanup(&endpoint, "rmp", id)
            .with_context(|| format!("removing nodelet-managed pod sandbox {id}"))?;
    }
    if !sandbox_ids.is_empty() {
        eprintln!(
            "nodemigrate: stopped and removed {} nodelet-managed pod sandbox(es); Kubernetes API objects and PV payloads remain available for destination reconciliation",
            sandbox_ids.len()
        );
    }
    Ok(())
}

fn capture_cilium_identity_at(
    installation: &Installation,
    endpoint: &str,
) -> Result<SourceCiliumIdentity> {
    let is_cilium = installation
        .cluster
        .as_ref()
        .and_then(|cluster| cluster.cni.as_deref())
        .is_some_and(|cni| cni.eq_ignore_ascii_case("cilium"));
    if !is_cilium {
        return Ok(SourceCiliumIdentity::default());
    }
    let pods = checked_output(
        "crictl",
        &["--runtime-endpoint", endpoint, "pods", "-o", "json"],
        "listing Cilium source pod identities",
    )?;
    let pods: serde_json::Value =
        serde_json::from_slice(&pods).context("parsing Cilium source pod identities")?;
    let (sandbox_ids, pod_uids) = cilium_source_sandbox_ids(&pods);
    let containers = checked_output(
        "crictl",
        &["--runtime-endpoint", endpoint, "ps", "-a", "-o", "json"],
        "listing Cilium source container identities",
    )?;
    let containers: serde_json::Value = serde_json::from_slice(&containers)
        .context("parsing Cilium source container identities")?;
    Ok(SourceCiliumIdentity {
        sandbox_ids,
        container_ids: cilium_host_container_ids(&containers),
        pod_uids,
    })
}

fn cilium_source_sandbox_ids(pods: &serde_json::Value) -> (Vec<String>, Vec<String>) {
    let mut sandbox_ids = Vec::new();
    let mut cilium_agent_sandbox_ids = Vec::new();
    let mut pod_uids = Vec::new();
    for pod in pods
        .get("items")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
    {
        let namespace_is_cilium = pod
            .pointer("/metadata/namespace")
            .and_then(serde_json::Value::as_str)
            == Some("kube-system");
        let pod_name = pod
            .pointer("/metadata/name")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        let pod_name_is_cilium = pod_name.starts_with("cilium");
        let cilium_app_label = pod
            .pointer("/labels/k8s-app")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|app| app == "cilium" || app == "cilium-envoy");
        let is_cilium = namespace_is_cilium && (pod_name_is_cilium || cilium_app_label);
        if !is_cilium {
            continue;
        }
        if let Some(id) = pod
            .pointer("/id")
            .and_then(serde_json::Value::as_str)
            .filter(|id| !id.is_empty())
        {
            if pod_name.starts_with("cilium-agent") {
                cilium_agent_sandbox_ids.push(id.to_string());
            } else {
                sandbox_ids.push(id.to_string());
            }
        }
        if let Some(uid) = pod
            .pointer("/metadata/uid")
            .and_then(serde_json::Value::as_str)
            .or_else(|| {
                pod.pointer("/labels/io.kubernetes.pod.uid")
                    .and_then(serde_json::Value::as_str)
            })
            .or_else(|| {
                pod.pointer("/labels/nodelet.dev/pod-uid")
                    .and_then(serde_json::Value::as_str)
            })
            .filter(|uid| !uid.is_empty())
        {
            pod_uids.push(uid.to_string());
        }
    }
    // Keep the Cilium agent alive until other Cilium sandboxes are removed;
    // CNI teardown for those sandboxes may still need the local agent.
    sandbox_ids.extend(cilium_agent_sandbox_ids);
    (sandbox_ids, pod_uids)
}

fn stop_cilium_source_sandboxes(
    installation: &Installation,
    identity: &SourceCiliumIdentity,
) -> Result<()> {
    let endpoint = std::env::var("NODEMIGRATE_CRI_ENDPOINT")
        .ok()
        .filter(|value| !value.is_empty())
        .or_else(|| installation.runtime_endpoint.clone())
        .unwrap_or_else(|| "unix:///run/containerd/containerd.sock".to_string());
    stop_cilium_sandboxes_at(&endpoint, identity, "source")
}

fn stop_cilium_sandboxes_at(
    endpoint: &str,
    identity: &SourceCiliumIdentity,
    description: &str,
) -> Result<()> {
    for id in &identity.sandbox_ids {
        checked_cri_cleanup(endpoint, "stopp", id)
            .with_context(|| format!("stopping {description} Cilium pod sandbox {id}"))?;
        checked_cri_cleanup(endpoint, "rmp", id)
            .with_context(|| format!("removing {description} Cilium pod sandbox {id}"))?;
    }
    Ok(())
}

fn checked_output(program: &str, args: &[&str], operation: &str) -> Result<Vec<u8>> {
    let output = command(program, args).with_context(|| operation.to_string())?;
    ensure!(
        output.status.success(),
        "{operation} failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(output.stdout)
}

pub fn disable(installation: &Installation) -> Result<PreviousServiceState> {
    let manager = installation
        .service_manager
        .context("source service manager could not be identified; refusing to stop the source")?;
    let name = &installation.service_name;
    if installation.distribution == crate::request::Distribution::Nodestore {
        let runtime = runtime_service_state(manager, installation)?;
        let nodelet_sandbox_ids = if runtime.is_some() {
            capture_nodelet_sandbox_ids(installation)?
        } else {
            Vec::new()
        };
        let cilium_identity = if runtime.is_some() {
            capture_source_cilium_identity(installation)?
        } else {
            SourceCiliumIdentity::default()
        };
        let mut previous = stop_nodestore_stack(manager, true)?;
        previous.runtime = runtime;
        if let Err(error) = stop_nodelet_source_sandboxes(installation, &nodelet_sandbox_ids) {
            if let Err(restore_error) = restore(installation, previous) {
                bail!(
                    "stopping nodestore workload sandboxes failed ({error:#}) and restoring nodestore failed ({restore_error:#})"
                );
            }
            return Err(error)
                .context("stopping nodestore workload sandboxes; nodestore was restored");
        }
        if let Some(runtime) = &previous.runtime {
            let runtime_name = runtime.name.clone();
            if let Err(error) = stop_and_disable(manager, &runtime_name) {
                if let Err(restore_error) = restore(installation, previous) {
                    bail!(
                        "stopping nodestore runtime '{runtime_name}' failed ({error:#}) and restoring nodestore failed ({restore_error:#})"
                    );
                }
                return Err(error).with_context(|| {
                    format!("stopping nodestore runtime {runtime_name}; nodestore was restored")
                });
            }
        }
        if let Err(error) = stop_orphaned_cilium_processes(installation, &cilium_identity) {
            if let Err(restore_error) = restore(installation, previous) {
                bail!(
                    "stopping nodestore Cilium processes failed ({error:#}) and restoring nodestore failed ({restore_error:#})"
                );
            }
            return Err(error)
                .context("stopping nodestore Cilium processes; nodestore was restored");
        }
        return Ok(previous);
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
                runtime: None,
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
                runtime: None,
            }
        }
        ServiceManager::SysVInit => PreviousServiceState {
            enabled: sysv_enabled(name),
            active: command("service", &[name, "status"])
                .is_ok_and(|output| output.status.success()),
            services: Vec::new(),
            runtime: None,
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
                runtime: None,
            }
        }
    };
    ensure!(
        previous.active,
        "source service '{}' is not active; refusing to migrate a stopped cluster",
        installation.service_name
    );
    let mut previous = previous;
    if installation.distribution == Distribution::Kubernetes {
        previous.runtime = runtime_service_state(manager, installation)?;
    }
    let cilium_identity = capture_source_cilium_identity(installation)?;
    if installation.distribution == Distribution::K3s {
        stop_cilium_source_sandboxes(installation, &cilium_identity)
            .context("tearing down source Cilium pod sandboxes before stopping K3s")?;
    }
    if let Err(error) = stop_and_disable(manager, name) {
        if let Err(restore_error) = restore(installation, previous) {
            bail!(
                "disabling source service failed ({error:#}) and restoring its previous state failed ({restore_error:#})"
            );
        }
        return Err(error).context("could not disable source service; previous state was restored");
    }
    if installation.distribution == Distribution::Kubernetes
        && installation.role == NodeRole::ControlPlane
    {
        if let Err(error) = stop_upstream_static_pods(installation) {
            if let Err(restore_error) = restore(installation, previous) {
                bail!(
                    "stopping source control-plane static pods failed ({error:#}) and restoring the source failed ({restore_error:#})"
                );
            }
            return Err(error)
                .context("stopping source control-plane static pods; source was restored");
        }
    }
    if installation.distribution == Distribution::Kubernetes {
        if let Err(error) = stop_cilium_source_sandboxes(installation, &cilium_identity) {
            if let Err(restore_error) = restore(installation, previous) {
                bail!(
                    "stopping source Cilium sandboxes failed ({error:#}) and restoring the source failed ({restore_error:#})"
                );
            }
            return Err(error)
                .context("stopping source Cilium sandboxes; source services were restarted");
        }
    }
    if let Some(runtime) = &previous.runtime {
        let runtime_name = runtime.name.clone();
        if let Err(error) = stop_and_disable(manager, &runtime_name) {
            if let Err(restore_error) = restore(installation, previous) {
                bail!(
                    "stopping source runtime '{runtime_name}' failed ({error:#}) and restoring the source failed ({restore_error:#})"
                );
            }
            return Err(error).with_context(|| {
                format!("stopping source runtime {runtime_name}; source was restored")
            });
        }
    }
    if let Err(error) = stop_orphaned_cilium_processes(installation, &cilium_identity) {
        if let Err(restore_error) = restore(installation, previous) {
            bail!(
                "stopping source Cilium processes failed ({error:#}) and restoring the source failed ({restore_error:#})"
            );
        }
        return Err(error).context("stopping source Cilium processes; source was restored");
    }
    Ok(previous)
}

/// Remove kubeadm control-plane static-pod sandboxes after kubelet is stopped.
/// Kubelet shutdown alone leaves those CRI containers bound to API/etcd ports.
pub fn stop_upstream_static_pods(installation: &Installation) -> Result<()> {
    stop_upstream_static_pods_inner(installation, true)
}

fn stop_upstream_static_pods_inner(
    installation: &Installation,
    require_static_pods: bool,
) -> Result<()> {
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
        !require_static_pods || !ids.is_empty(),
        "no kubeadm control-plane static pod sandboxes were found after stopping kubelet"
    );
    tracing::info!(count = ids.len(), "stopping kubeadm static pod sandboxes");
    for id in &ids {
        checked_cri_cleanup(&endpoint, "stopp", id)
            .with_context(|| format!("stopping source static pod sandbox {id}"))?;
        checked_cri_cleanup(&endpoint, "rmp", id)
            .with_context(|| format!("removing source static pod sandbox {id}"))?;
    }
    wait_for_api_port_release()
}

/// Stop a partially started retained Kubernetes installation before restoring
/// the prior nodestore stack after a failed reverse migration. The destination
/// must be quiesced first so its API server, CNI, and workloads cannot conflict
/// with the restored source services or mutate the recovery snapshot.
pub fn stop_reverse_migration_target(installation: &Installation) -> Result<()> {
    ensure!(
        matches!(
            installation.distribution,
            Distribution::K3s | Distribution::Kubernetes
        ),
        "reverse migration rollback requires a retained K3s or Kubernetes target"
    );
    let manager = installation
        .service_manager
        .context("target service manager is unknown; cannot roll back the retained target")?;
    if service_active(manager, &installation.service_name) {
        return disable(installation)
            .map(|_| ())
            .context("stopping the retained target stack during reverse-migration rollback");
    }

    let runtime_name = (installation.distribution == Distribution::Kubernetes)
        .then(|| installation_runtime_service_name(installation))
        .transpose()?;
    let runtime_active = runtime_name
        .as_deref()
        .is_some_and(|name| service_active(manager, name));
    let endpoint = installation_runtime_endpoint(installation);
    let cri_available = runtime_active && runtime_endpoint_available(&endpoint);
    let identity = if cri_available {
        capture_cilium_identity_at(installation, &endpoint)
            .context("capturing retained target Cilium identities for rollback")?
    } else {
        SourceCiliumIdentity::default()
    };
    if installation.distribution == Distribution::K3s {
        stop_cilium_source_sandboxes(installation, &identity)
            .context("stopping retained K3s Cilium sandboxes during rollback")?;
    }
    stop_and_disable(manager, &installation.service_name).with_context(|| {
        format!(
            "stopping retained target service {}",
            installation.service_name
        )
    })?;
    if installation.distribution == Distribution::Kubernetes {
        if cri_available && installation.role == NodeRole::ControlPlane {
            stop_upstream_static_pods_inner(installation, false)
                .context("stopping partial retained kubeadm control-plane sandboxes")?;
        }
        if cri_available {
            stop_cilium_source_sandboxes(installation, &identity)
                .context("stopping retained Kubernetes Cilium sandboxes during rollback")?;
        }
        if let Some(runtime_name) = runtime_name {
            if service_exists(manager, &runtime_name) && service_active(manager, &runtime_name) {
                stop_and_disable(manager, &runtime_name).with_context(|| {
                    format!("stopping retained target runtime {runtime_name} during rollback")
                })?;
            }
        }
    }
    stop_orphaned_cilium_processes(installation, &identity)
        .context("stopping retained target Cilium processes during rollback")?;
    Ok(())
}

/// Source Cilium host-network containers can leave their agent, operator, or
/// Envoy processes alive after their sandboxes stop. Retain exact source
/// container and Pod identities; signal only known Cilium daemons attached to
/// those identities so unrelated or destination processes remain untouched.
#[cfg(unix)]
pub(crate) fn stop_orphaned_cilium_processes(
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
        eprintln!(
            "nodemigrate: stopped {stopped} leftover source Cilium daemon process(es) by exact container or pod identity"
        );
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
pub(crate) fn stop_orphaned_cilium_processes(
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
                    .with_context(|| format!("reading command line for process {pid}"));
            }
        };
        if !is_cilium_host_process(&command_line) {
            continue;
        }
        let cgroup = match std::fs::read_to_string(entry.path().join("cgroup")) {
            Ok(cgroup) => cgroup,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(error).with_context(|| format!("reading cgroup for process {pid}"));
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
                return Err(error).with_context(|| format!("reading process stat for {current}"));
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
                    .with_context(|| format!("reading command line for process {parent_pid}"));
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
            return Err(error).with_context(|| format!("reading {}", directory.display()));
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
            let control_plane_name = pod
                .pointer("/metadata/name")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|name| {
                    [
                        "kube-apiserver-",
                        "kube-controller-manager-",
                        "kube-scheduler-",
                        "etcd-",
                    ]
                    .iter()
                    .any(|prefix| name.starts_with(prefix))
                });
            namespace == Some("kube-system") && control_plane_name
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
    if let Some(runtime) = &previous.runtime {
        restore_named(manager, runtime)
            .with_context(|| format!("restoring source runtime service {}", runtime.name))?;
    }
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
    let target_cri = std::path::Path::new("/run/containerd/containerd.sock");
    let cilium_identity = if service_active(manager, "containerd") && target_cri.exists() {
        capture_cilium_identity_at(installation, "unix:///run/containerd/containerd.sock")
            .context("capturing partial nodestore Cilium identities for rollback")?
    } else {
        SourceCiliumIdentity::default()
    };
    stop_nodestore_stack(manager, false)?;
    stop_nodestore_runtime_for_rollback(
        installation,
        manager,
        &cilium_identity,
        "unix:///run/containerd/containerd.sock",
    )?;
    Ok(())
}

fn stop_nodestore_runtime_for_rollback(
    installation: &Installation,
    manager: ServiceManager,
    cilium_identity: &SourceCiliumIdentity,
    runtime_endpoint: &str,
) -> Result<()> {
    stop_cilium_sandboxes_at(runtime_endpoint, cilium_identity, "partial nodestore")
        .context("stopping partial nodestore Cilium sandboxes")?;
    if service_exists(manager, "containerd") && service_active(manager, "containerd") {
        stop_and_disable(manager, "containerd")
            .context("stopping the partial nodestore containerd runtime")?;
    }
    stop_orphaned_cilium_processes(installation, cilium_identity)
        .context("stopping partial nodestore Cilium processes")?;
    Ok(())
}

/// Stop only the local node services and runtime installed by a failed worker
/// join. The remote nodestore control-plane services belong to the cluster and
/// must remain available.
pub fn stop_nodestore_worker_for_rollback(installation: &Installation) -> Result<()> {
    let manager = installation
        .service_manager
        .context("service manager is unknown; cannot stop the partial nodestore worker")?;
    let destination_endpoint = "unix:///run/containerd/containerd.sock";
    let cilium_identity = if service_active(manager, "containerd")
        && std::path::Path::new("/run/containerd/containerd.sock").exists()
    {
        capture_cilium_identity_at(installation, destination_endpoint)
            .context("capturing partial nodestore worker Cilium identities for rollback")?
    } else {
        SourceCiliumIdentity::default()
    };
    for service in ["nodelet", "nodeproxy", "flanneld"] {
        if service_exists(manager, service) && service_active(manager, service) {
            stop_and_disable(manager, service)
                .with_context(|| format!("stopping partial nodestore worker service {service}"))?;
        }
    }
    stop_nodestore_runtime_for_rollback(
        installation,
        manager,
        &cilium_identity,
        destination_endpoint,
    )
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
        !require_active || service_stack_is_active_or_staged(&services),
        "no active nodebootstrap services were found to stop"
    );
    let previous = PreviousServiceState {
        enabled: false,
        active: false,
        services,
        runtime: None,
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
                    bail!(
                        "stopping nodebootstrap service '{}' failed ({error:#}) and restoring the stack failed ({restore_error:#})",
                        service.name
                    );
                }
                return Err(error)
                    .with_context(|| format!("stopping nodebootstrap service {}", service.name));
            }
        }
    }
    Ok(previous)
}

fn service_stack_is_active_or_staged(services: &[ServiceState]) -> bool {
    services.iter().any(|service| service.active)
        || (!services.is_empty()
            && services
                .iter()
                .all(|service| !service.active && !service.enabled))
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
    if installation.distribution == Distribution::Kubernetes {
        let runtime_name = installation_runtime_service_name(installation)?;
        ensure!(
            service_exists(manager, &runtime_name),
            "retained Kubernetes runtime service '{runtime_name}' was not found"
        );
        restore_named(
            manager,
            &ServiceState { name: runtime_name, enabled: true, active: true },
        )
        .context("starting the retained Kubernetes container runtime before kubelet")?;
    }
    restore_named(
        manager,
        &ServiceState {
            name: installation.service_name.clone(),
            enabled: true,
            active: true,
        },
    )
}

/// Restart the retained cluster service after deleting this host's old Node
/// object. A kubelet that was already registered may not recreate a Node after
/// its API object is deleted until the kubelet process registers again.
pub fn restart(installation: &Installation) -> Result<()> {
    let manager = installation
        .service_manager
        .context("target service manager could not be identified")?;
    ensure!(
        installation.distribution != crate::request::Distribution::Nodestore,
        "restarting an existing nodestore target requires restoring its full stack"
    );
    restart_named(manager, &installation.service_name)
}

fn restart_named(manager: ServiceManager, name: &str) -> Result<()> {
    match manager {
        ServiceManager::Systemd => checked("systemctl", &["restart", &format!("{name}.service")]),
        ServiceManager::OpenRc => checked("rc-service", &[name, "restart"]),
        ServiceManager::SysVInit => checked("service", &[name, "restart"]),
        ServiceManager::Runit => {
            let dir = runit_service_dir(name)
                .with_context(|| format!("could not find runit service directory for {name}"))?;
            checked("sv", &["restart", &dir.to_string_lossy()])
        }
    }
}

/// Pause the local Kubernetes Pod reconcilers while CRI sandboxes are removed.
/// A live kubelet/Nodelet can recreate a container between StopPodSandbox and
/// RemoveContainer, racing the cleanup that is resetting its own sandboxes.
/// K3s embeds its API server and kubelet in the main process. Keep that process
/// live during CNI teardown: Cilium's CNI plugin can need the Kubernetes API
/// while StopPodSandbox waits for DEL to finish. Freezing K3s also freezes API.
pub(crate) fn with_local_pod_agents_paused<T>(
    runtime_endpoint: &str,
    operation: impl FnOnce() -> Result<T>,
) -> Result<T> {
    let controls = local_pod_agent_controls(runtime_endpoint)?;
    with_pod_agent_controls_paused(&controls, 0, operation)
}

#[derive(Debug, Clone)]
enum PodAgentControl {
    Service {
        manager: ServiceManager,
        name: String,
    },
    K3sMainProcess,
}

fn local_pod_agent_controls(runtime_endpoint: &str) -> Result<Vec<PodAgentControl>> {
    let installations = crate::detect::inspect_all(&crate::detect::HostLayout::system())?;
    let mut controls = Vec::new();
    for installation in installations
        .iter()
        .filter(|installation| pod_agent_uses_runtime(installation, runtime_endpoint))
    {
        let Some(manager) = installation.service_manager else {
            continue;
        };
        if !service_active(manager, &installation.service_name) {
            continue;
        }
        let control = match installation.distribution {
            Distribution::Kubernetes => PodAgentControl::Service {
                manager,
                name: installation.service_name.clone(),
            },
            Distribution::K3s => {
                let endpoint = installation.runtime_endpoint.as_deref().unwrap_or_default();
                if endpoint.contains("/run/k3s/containerd/containerd.sock") {
                    PodAgentControl::K3sMainProcess
                } else {
                    PodAgentControl::Service {
                        manager,
                        name: installation.service_name.clone(),
                    }
                }
            }
            Distribution::Nodestore => continue,
        };
        controls.push(control);
    }
    // An installed kubelet that matches the runtime but is stopped cannot
    // race CRI cleanup. Nodelet may still own Pods on that same endpoint
    // (for example, while replacing an upstream kubelet), so fall back to
    // pausing it whenever no active matching upstream agent was selected.
    if should_pause_nodelet_fallback(&controls) {
        if let Some(manager) = crate::detect::nodelet_service_manager() {
            if service_active(manager, "nodelet") {
                controls.push(PodAgentControl::Service {
                    manager,
                    name: "nodelet".to_string(),
                });
            }
        }
    }
    controls.sort_by(|left, right| pod_agent_control_name(left).cmp(pod_agent_control_name(right)));
    controls.dedup_by(|left, right| pod_agent_control_name(left) == pod_agent_control_name(right));
    Ok(controls)
}

fn should_pause_nodelet_fallback(active_matching_controls: &[PodAgentControl]) -> bool {
    active_matching_controls.is_empty()
}

fn pod_agent_uses_runtime(installation: &Installation, runtime_endpoint: &str) -> bool {
    let Some(installed_endpoint) = installation.runtime_endpoint.as_deref().or_else(|| {
        (installation.distribution == Distribution::Kubernetes)
            .then_some("unix:///run/containerd/containerd.sock")
    }) else {
        return false;
    };
    matches!(
        installation.distribution,
        Distribution::K3s | Distribution::Kubernetes
    ) && installed_endpoint == runtime_endpoint
}

fn pod_agent_control_name(control: &PodAgentControl) -> &str {
    match control {
        PodAgentControl::Service { name, .. } => name,
        PodAgentControl::K3sMainProcess => "k3s",
    }
}

fn with_pod_agent_controls_paused<T>(
    controls: &[PodAgentControl],
    index: usize,
    operation: impl FnOnce() -> Result<T>,
) -> Result<T> {
    let Some(control) = controls.get(index) else {
        return operation();
    };
    // The K3s main process also serves its API. Pausing it makes Cilium CNI
    // DEL wait on the API that this migration has just frozen.
    if !pod_agent_control_should_pause(control) {
        return with_pod_agent_controls_paused(controls, index + 1, operation);
    }
    with_service_paused(
        true,
        || {
            eprintln!(
                "nodemigrate: pausing local {} while removing CRI Pod sandboxes",
                pod_agent_control_name(control)
            );
            stop_pod_agent_control(control)
        },
        || with_pod_agent_controls_paused(controls, index + 1, operation),
        || start_pod_agent_control(control),
    )
}

fn pod_agent_control_should_pause(control: &PodAgentControl) -> bool {
    !matches!(control, PodAgentControl::K3sMainProcess)
}

fn stop_pod_agent_control(control: &PodAgentControl) -> Result<()> {
    match control {
        PodAgentControl::Service { manager, name } => stop_named(*manager, name)
            .with_context(|| format!("pausing local {name} before CRI sandbox cleanup")),
        PodAgentControl::K3sMainProcess => {
            // The wrapper skips K3s controls instead of freezing the combined
            // API/kubelet process while its CNI plugin is still active.
            Ok(())
        }
    }
}

fn start_pod_agent_control(control: &PodAgentControl) -> Result<()> {
    match control {
        PodAgentControl::Service { manager, name } => start_named(*manager, name)
            .with_context(|| format!("restarting local {name} after CRI sandbox cleanup")),
        PodAgentControl::K3sMainProcess => Ok(()),
    }
}

fn with_service_paused<T>(
    should_pause: bool,
    stop: impl FnOnce() -> Result<()>,
    operation: impl FnOnce() -> Result<T>,
    start: impl FnOnce() -> Result<()>,
) -> Result<T> {
    if !should_pause {
        return operation();
    }
    stop()?;
    let operation_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(operation));
    let start_result = start();
    match (operation_result, start_result) {
        (Ok(Ok(value)), Ok(())) => {
            eprintln!("nodemigrate: resumed local Pod reconciliation after CRI sandbox cleanup");
            Ok(value)
        }
        (Ok(Err(operation_error)), Ok(())) => Err(operation_error),
        (Ok(Ok(_)), Err(start_error)) => Err(start_error),
        (Ok(Err(operation_error)), Err(start_error)) => Err(operation_error).context(format!(
            "resuming local Pod agent also failed: {start_error:#}"
        )),
        (Err(payload), Ok(())) => std::panic::resume_unwind(payload),
        (Err(payload), Err(start_error)) => {
            eprintln!(
                "nodemigrate: resuming Pod reconciliation after panic also failed: {start_error:#}"
            );
            std::panic::resume_unwind(payload)
        }
    }
}

fn stop_named(manager: ServiceManager, name: &str) -> Result<()> {
    match manager {
        ServiceManager::Systemd => checked("systemctl", &["stop", &format!("{name}.service")]),
        ServiceManager::OpenRc => checked("rc-service", &[name, "stop"]),
        ServiceManager::SysVInit => checked("service", &[name, "stop"]),
        ServiceManager::Runit => {
            let dir = runit_service_dir(name)
                .with_context(|| format!("could not find runit service directory for {name}"))?;
            checked("sv", &["down", &dir.to_string_lossy()])
        }
    }
}

fn start_named(manager: ServiceManager, name: &str) -> Result<()> {
    match manager {
        ServiceManager::Systemd => checked("systemctl", &["start", &format!("{name}.service")]),
        ServiceManager::OpenRc => checked("rc-service", &[name, "start"]),
        ServiceManager::SysVInit => checked("service", &[name, "start"]),
        ServiceManager::Runit => {
            let dir = runit_service_dir(name)
                .with_context(|| format!("could not find runit service directory for {name}"))?;
            checked("sv", &["up", &dir.to_string_lossy()])
        }
    }
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

pub(crate) fn checked_cri_cleanup(endpoint: &str, action: &str, id: &str) -> Result<bool> {
    let mut attempts = 0;
    loop {
        attempts += 1;
        let output = command(
            "crictl",
            &[
                "--timeout",
                CRICTL_CLEANUP_TIMEOUT,
                "--runtime-endpoint",
                endpoint,
                action,
                id,
            ],
        )?;
        if cri_cleanup_succeeded(output.status.success(), &output.stderr) {
            return Ok(output.status.success());
        }
        if action == "stopp"
            && cri_cleanup_retryable(action, &output.stderr)
            && cri_pod_sandbox_stopped(endpoint, id)
        {
            eprintln!(
                "nodemigrate: StopPodSandbox timed out for {id}, but CRI confirms the sandbox is already SANDBOX_NOTREADY; continuing cleanup"
            );
            return Ok(true);
        }
        let attempt_limit = cri_cleanup_attempt_limit(action, &output.stderr);
        if attempts >= attempt_limit {
            bail!(
                "crictl {action} failed for {id} at {endpoint}: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        std::thread::sleep(Duration::from_secs(1));
    }
}

fn cri_pod_sandbox_stopped(endpoint: &str, id: &str) -> bool {
    let Ok(output) = command(
        "crictl",
        &[
            "--timeout",
            "10s",
            "--runtime-endpoint",
            endpoint,
            "inspectp",
            "-o",
            "json",
            id,
        ],
    ) else {
        return false;
    };
    output.status.success() && cri_pod_sandbox_status_is_stopped(&output.stdout)
}

fn cri_pod_sandbox_status_is_stopped(status: &[u8]) -> bool {
    let Ok(status) = serde_json::from_slice::<serde_json::Value>(status) else {
        return false;
    };
    matches!(
        status.pointer("/status/state"),
        Some(serde_json::Value::String(state)) if state == "SANDBOX_NOTREADY"
    ) || matches!(status.pointer("/status/state"), Some(serde_json::Value::Number(state)) if state.as_i64() == Some(1))
}

fn cri_cleanup_succeeded(exit_success: bool, stderr: &[u8]) -> bool {
    exit_success || cri_not_found(stderr)
}

fn cri_cleanup_retryable(action: &str, stderr: &[u8]) -> bool {
    let message = String::from_utf8_lossy(stderr).to_ascii_lowercase();
    let deadline_expired =
        message.contains("deadlineexceeded") || message.contains("context deadline exceeded");
    (matches!(action, "stop" | "stopp" | "rmp") && deadline_expired)
        || (action == "rm" && message.contains("container is in starting state"))
}

fn cri_cleanup_attempt_limit(action: &str, stderr: &[u8]) -> usize {
    if !cri_cleanup_retryable(action, stderr) {
        1
    } else if action == "rm" {
        CRICTL_CLEANUP_TRANSITION_ATTEMPTS
    } else {
        CRICTL_CLEANUP_DEADLINE_ATTEMPTS
    }
}

fn cri_not_found(stderr: &[u8]) -> bool {
    let message = String::from_utf8_lossy(stderr).to_ascii_lowercase();
    message.contains("code = notfound")
        || message.contains("code = not found")
        || (message.contains("not found")
            && (message.contains("container") || message.contains("sandbox")))
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
        ServiceState, SourceCiliumIdentity, cilium_host_container_ids, cilium_source_sandbox_ids,
        cri_cleanup_attempt_limit, cri_cleanup_retryable, cri_cleanup_succeeded,
        cri_pod_sandbox_status_is_stopped,
        nodelet_source_sandbox_ids, pod_agent_control_should_pause, pod_agent_uses_runtime,
        service_stack_is_active_or_staged, should_pause_nodelet_fallback,
        runtime_service_name, static_pod_sandbox_ids, with_service_paused,
    };
    use crate::detect::{Installation, NodeRole, ServiceManager};
    use crate::request::Distribution;

    const SOURCE_CONTAINER_ID: &str =
        "ef96e5cf937fed840c1bfcc03df0ef667927c7f666ba4963da35faaa9f80f39a";

    #[test]
    fn staged_nodestore_services_can_be_disabled_again() {
        let staged = vec![ServiceState {
            name: "nodestore".to_string(),
            enabled: false,
            active: false,
        }];
        assert!(service_stack_is_active_or_staged(&staged));

        let unexpectedly_inactive = vec![ServiceState {
            name: "nodestore".to_string(),
            enabled: true,
            active: false,
        }];
        assert!(!service_stack_is_active_or_staged(&unexpectedly_inactive));
        assert!(!service_stack_is_active_or_staged(&[]));
    }

    #[test]
    fn only_pauses_the_pod_agent_using_the_cleaned_runtime() {
        let installation = |distribution, runtime_endpoint: Option<&str>| Installation {
            distribution,
            role: NodeRole::Worker,
            runtime_endpoint: runtime_endpoint.map(str::to_string),
            service_manager: Some(ServiceManager::Systemd),
            service_name: "test-agent".to_string(),
            service_file: None,
            binary: None,
            config_files: Vec::new(),
            cluster: None,
        };

        assert!(pod_agent_uses_runtime(
            &installation(
                Distribution::K3s,
                Some("unix:///run/k3s/containerd/containerd.sock")
            ),
            "unix:///run/k3s/containerd/containerd.sock"
        ));
        assert!(pod_agent_uses_runtime(
            &installation(
                Distribution::Kubernetes,
                Some("unix:///run/containerd/containerd.sock")
            ),
            "unix:///run/containerd/containerd.sock"
        ));
        assert!(pod_agent_uses_runtime(
            &installation(Distribution::Kubernetes, None),
            "unix:///run/containerd/containerd.sock"
        ));
        assert!(!pod_agent_uses_runtime(
            &installation(
                Distribution::K3s,
                Some("unix:///run/k3s/containerd/containerd.sock")
            ),
            "unix:///run/containerd/containerd.sock"
        ));
        assert!(!pod_agent_uses_runtime(
            &installation(
                Distribution::Nodestore,
                Some("unix:///run/containerd/containerd.sock")
            ),
            "unix:///run/containerd/containerd.sock"
        ));
    }

    #[test]
    fn falls_back_to_nodelet_when_matching_upstream_agent_is_not_active() {
        assert!(should_pause_nodelet_fallback(&[]));
        assert!(!should_pause_nodelet_fallback(&[super::PodAgentControl::Service {
            manager: ServiceManager::Systemd,
            name: "kubelet".to_string(),
        }]));
    }

    #[test]
    fn keeps_k3s_api_live_during_cni_sandbox_cleanup() {
        assert!(!pod_agent_control_should_pause(
            &super::PodAgentControl::K3sMainProcess
        ));
        assert!(pod_agent_control_should_pause(
            &super::PodAgentControl::Service {
                manager: ServiceManager::Systemd,
                name: "kubelet".to_string(),
            }
        ));
    }

    #[test]
    fn resumes_service_after_paused_operation_fails() {
        use std::cell::Cell;

        let stopped = Cell::new(false);
        let restarted = Cell::new(false);
        let result: anyhow::Result<()> = with_service_paused(
            true,
            || {
                stopped.set(true);
                Ok(())
            },
            || anyhow::bail!("cleanup failed"),
            || {
                assert!(stopped.get());
                restarted.set(true);
                Ok(())
            },
        );

        assert!(result.is_err());
        assert!(restarted.get());
    }

    #[test]
    fn failed_service_stop_prevents_paused_operation() {
        use std::cell::Cell;

        let operation_ran = Cell::new(false);
        let restarted = Cell::new(false);
        let result: anyhow::Result<()> = with_service_paused(
            true,
            || anyhow::bail!("stop failed"),
            || {
                operation_ran.set(true);
                Ok(())
            },
            || {
                restarted.set(true);
                Ok(())
            },
        );

        assert!(result.is_err());
        assert!(!operation_ran.get());
        assert!(!restarted.get());
    }

    #[test]
    fn leaves_inactive_service_untouched() {
        use std::cell::Cell;

        let service_commands_ran = Cell::new(false);
        let result = with_service_paused(
            false,
            || {
                service_commands_ran.set(true);
                Ok(())
            },
            || Ok(42),
            || {
                service_commands_ran.set(true);
                Ok(())
            },
        )
        .unwrap();

        assert_eq!(result, 42);
        assert!(!service_commands_ran.get());
    }

    #[test]
    fn treats_concurrent_cri_removal_as_success_but_preserves_other_errors() {
        assert!(cri_cleanup_succeeded(true, b""));
        assert!(cri_cleanup_succeeded(
            false,
            b"rpc error: code = NotFound desc = container not found"
        ));
        assert!(cri_cleanup_succeeded(
            false,
            b"getting sandbox status: sandbox: not found"
        ));
        assert!(!cri_cleanup_succeeded(
            false,
            b"rpc error: code = Unavailable desc = connection refused"
        ));
        assert!(!cri_cleanup_succeeded(false, b"permission denied"));
    }

    #[test]
    fn retries_only_cri_cleanup_state_transitions_and_deadlines() {
        assert!(cri_cleanup_retryable(
            "stopp",
            b"rpc error: code = DeadlineExceeded desc = context deadline exceeded"
        ));
        assert!(cri_cleanup_retryable(
            "stop",
            b"rpc error: code = DeadlineExceeded desc = context deadline exceeded"
        ));
        assert!(cri_cleanup_retryable(
            "rmp",
            b"rpc error: code = DeadlineExceeded desc = context deadline exceeded"
        ));
        assert!(cri_cleanup_retryable(
            "rm",
            b"container is in starting state, can't be removed"
        ));
        assert!(!cri_cleanup_retryable(
            "rm",
            b"rpc error: code = DeadlineExceeded desc = context deadline exceeded"
        ));
        assert!(!cri_cleanup_retryable("stopp", b"permission denied"));
        assert_eq!(
            cri_cleanup_attempt_limit("rm", b"container is in starting state"),
            16
        );
        assert_eq!(
            cri_cleanup_attempt_limit("stopp", b"context deadline exceeded"),
            2
        );
        assert_eq!(cri_cleanup_attempt_limit("stopp", b"permission denied"), 1);
    }

    #[test]
    fn accepts_cri_stop_timeout_only_when_inspection_confirms_notready() {
        assert!(cri_pod_sandbox_status_is_stopped(
            br#"{"status":{"state":"SANDBOX_NOTREADY"}}"#
        ));
        assert!(cri_pod_sandbox_status_is_stopped(
            br#"{"status":{"state":1}}"#
        ));
        assert!(!cri_pod_sandbox_status_is_stopped(
            br#"{"status":{"state":"SANDBOX_READY"}}"#
        ));
        assert!(!cri_pod_sandbox_status_is_stopped(b"not json"));
    }

    #[test]
    fn maps_supported_cri_endpoints_to_their_host_service() {
        assert_eq!(
            runtime_service_name(Some("unix:///run/containerd/containerd.sock")),
            Some("containerd".to_string())
        );
        assert_eq!(
            runtime_service_name(Some("unix:///run/crio/crio.sock")),
            Some("crio".to_string())
        );
        assert_eq!(
            runtime_service_name(Some("unix:///run/cri-dockerd.sock")),
            Some("docker".to_string())
        );
        assert_eq!(runtime_service_name(None), Some("containerd".to_string()));
        assert_eq!(
            runtime_service_name(Some("unix:///custom/runtime.sock")),
            None
        );
    }

    #[test]
    fn selects_all_kubeadm_control_plane_sandboxes() {
        let pods = serde_json::json!({
            "items": [
                {
                    "id": "apiserver-sandbox",
                    "metadata": {"name": "kube-apiserver-node-a", "namespace": "kube-system"}
                },
                {
                    "id": "etcd-sandbox",
                    "metadata": {"name": "etcd-node-a", "namespace": "kube-system"}
                },
                {
                    "id": "controller-manager-sandbox",
                    "metadata": {"name": "kube-controller-manager-node-a", "namespace": "kube-system"}
                },
                {
                    "id": "scheduler-sandbox",
                    "metadata": {"name": "kube-scheduler-node-a", "namespace": "kube-system"}
                },
                {
                    "id": "coredns-sandbox",
                    "metadata": {"name": "coredns-abc", "namespace": "kube-system"}
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
            [
                "apiserver-sandbox",
                "etcd-sandbox",
                "controller-manager-sandbox",
                "scheduler-sandbox"
            ]
        );
    }

    #[test]
    fn selects_only_cilium_sandboxes_for_targeted_cleanup() {
        let pods = serde_json::json!({
            "items": [
                {"id": "cilium-agent-id", "metadata": {"name": "cilium-agent-node-a", "namespace": "kube-system", "uid": "agent-uid"}},
                {"id": "cilium-envoy-id", "metadata": {"name": "cilium-envoy-node-a", "namespace": "kube-system", "uid": "envoy-uid"}},
                {"id": "cilium-operator-id", "metadata": {"name": "cilium-operator-abc", "namespace": "kube-system", "uid": "operator-uid"}},
                {"id": "workload-id", "metadata": {"name": "app", "namespace": "apps", "uid": "workload-uid"}},
                {"id": "other-system-id", "metadata": {"name": "cilium-agent", "namespace": "default", "uid": "other-uid"}}
            ]
        });

        assert_eq!(
            cilium_source_sandbox_ids(&pods),
            (
                vec![
                    "cilium-envoy-id".to_string(),
                    "cilium-operator-id".to_string(),
                    "cilium-agent-id".to_string()
                ],
                vec![
                    "agent-uid".to_string(),
                    "envoy-uid".to_string(),
                    "operator-uid".to_string()
                ]
            )
        );
    }

    #[test]
    fn selects_only_nodelet_managed_sandboxes_for_cutover_restart() {
        let pods = serde_json::json!({
            "items": [
                {
                    "id": "nodelet-app",
                    "labels": {"nodelet.dev/pod-uid": "pod-uid", "nodelet.dev/pod-name": "app"}
                },
                {
                    "id": "nodelet-cilium",
                    "labels": {"nodelet.dev/pod-uid": "cilium-uid", "nodelet.dev/pod-name": "cilium-agent"}
                },
                {
                    "id": "kubelet-app",
                    "labels": {"io.kubernetes.pod.uid": "upstream-pod-uid"}
                },
                {"id": "unrelated-runtime-sandbox", "labels": {"app": "database"}},
                {"labels": {"nodelet.dev/pod-uid": "missing-id"}}
            ]
        });

        assert_eq!(
            nodelet_source_sandbox_ids(&pods),
            ["nodelet-app", "nodelet-cilium"]
        );
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
                    sandbox_ids: Vec::new(),
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
