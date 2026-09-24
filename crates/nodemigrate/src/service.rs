use std::process::{Command, Output};

use anyhow::{bail, ensure, Context, Result};

use crate::detect::{Installation, ServiceManager};

#[derive(Debug, Clone, Copy)]
pub struct PreviousServiceState {
    enabled: bool,
    active: bool,
}

pub fn require_supported_uninstall(installation: &Installation) -> Result<()> {
    ensure!(
        installation.distribution == crate::request::Distribution::K3s,
        "uninstall-after-migrate is currently supported only for K3s installations"
    );
    ensure!(
        std::path::Path::new("/usr/local/bin/k3s-uninstall.sh").is_file(),
        "uninstall-after-migrate=true requires /usr/local/bin/k3s-uninstall.sh"
    );
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
    let previous = match manager {
        ServiceManager::Systemd => {
            let enabled = command("systemctl", &["is-enabled", &format!("{name}.service")])
                .is_ok_and(|output| output.status.success());
            let active = command("systemctl", &["is-active", &format!("{name}.service")])
                .is_ok_and(|output| output.status.success());
            PreviousServiceState { enabled, active }
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
            }
        }
        ServiceManager::SysVInit => PreviousServiceState {
            enabled: sysv_enabled(name),
            active: command("service", &[name, "status"])
                .is_ok_and(|output| output.status.success()),
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

pub fn restore(installation: &Installation, previous: PreviousServiceState) -> Result<()> {
    let manager = installation
        .service_manager
        .context("source service manager is unknown")?;
    let name = &installation.service_name;
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

pub fn uninstall_k3s() -> Result<()> {
    checked("/usr/local/bin/k3s-uninstall.sh", &[])
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
