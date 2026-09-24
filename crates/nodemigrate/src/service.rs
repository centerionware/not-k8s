use std::process::{Command, Output};

use anyhow::{bail, ensure, Context, Result};

use crate::detect::{Distribution, Installation, NodeRole, ServiceManager};

#[derive(Debug, Clone)]
pub struct PreviousServiceState {
    enabled: bool,
    active: bool,
    services: Vec<ServiceState>,
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
        let mut services = Vec::new();
        for name in [
            "nodelet",
            "nodeproxy",
            "nodecontroller",
            "nodescheduler",
            "flanneld",
            "kube-apiserver",
            "nodeapiserver",
            "nodestore",
        ] {
            if service_exists(manager, name) {
                services.push(ServiceState {
                    name: name.to_string(),
                    enabled: service_enabled(manager, name),
                    active: service_active(manager, name),
                });
            }
        }
        ensure!(
            services.iter().any(|service| service.active),
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
                    if let Err(restore_error) = restore(installation, previous.clone()) {
                        bail!("stopping nodebootstrap service '{}' failed ({error:#}) and restoring the stack failed ({restore_error:#})", service.name);
                    }
                    return Err(error).with_context(|| {
                        format!("stopping nodebootstrap service {}", service.name)
                    });
                }
            }
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
    for id in &ids {
        checked("crictl", &["--runtime-endpoint", &endpoint, "stopp", id])
            .with_context(|| format!("stopping source static pod sandbox {id}"))?;
        checked("crictl", &["--runtime-endpoint", &endpoint, "rmp", id])
            .with_context(|| format!("removing source static pod sandbox {id}"))?;
    }
    Ok(())
}

fn static_pod_sandbox_ids(pods: &serde_json::Value) -> Vec<String> {
    pods.get("items")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter(|pod| {
            pod.pointer("/metadata/namespace")
                .and_then(serde_json::Value::as_str)
                == Some("kube-system")
                && pod
                    .pointer("/labels/kubernetes.io~1config.source")
                    .and_then(serde_json::Value::as_str)
                    == Some("file")
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
        // Restart dependencies in the opposite order from shutdown: datastore,
        // apiserver and network first, then controllers and node services.
        for service in previous.services.iter().rev() {
            restore_named(manager, service)?;
        }
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
    use super::static_pod_sandbox_ids;

    #[test]
    fn selects_only_kube_system_file_static_pod_sandboxes() {
        let pods = serde_json::json!({
            "items": [
                {
                    "id": "static-sandbox",
                    "metadata": {"namespace": "kube-system"},
                    "labels": {"kubernetes.io/config.source": "file"}
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
        assert_eq!(static_pod_sandbox_ids(&pods), ["static-sandbox"]);
    }
}
