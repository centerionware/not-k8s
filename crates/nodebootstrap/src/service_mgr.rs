//! Generic persistent-service install/remove — replaces
//! `deploy/lib/service-mgr.sh`. Shared by `containerd.rs` (when it starts
//! containerd itself), `cni.rs` (`flanneld`), and eventually
//! `targets/upstream.rs` (the three upstream control-plane binaries).
//!
//! **The same three-tier strategy as the shell version, all three ported —
//! this is not a systemd-only module.** systemd (`Restart=always`, enabled
//! on boot) -> OpenRC (`supervise-daemon`, respawn, added to boot) -> a
//! self-restarting background loop + `cron @reboot` as a last resort,
//! clearly logged as not a real service rather than silently accepted as
//! good enough. Every other module in this crate that currently bails with
//! "no OpenRC/non-systemd service writer ported yet" (`containerd.rs`,
//! `cni.rs`) gets that gap closed by wiring this module in as a follow-up
//! commit, not by this module itself reaching into them.
//!
//! Learned the hard way (same lesson `service-mgr.sh`'s header records):
//! a plain `nohup`'d process silently dies on any crash/reboot/terminal-
//! close with nothing to bring it back.

use anyhow::{Context, Result};

use crate::config::Config;

/// `exec_cmd` MUST be an absolute path to the binary, never a bare command
/// name -- systemd/OpenRC services get a fresh, minimal `PATH` that won't
/// include wherever this crate put a fetched/built binary
/// (`Config::toolchain_dir`). Confirmed for real against `flanneld` by the
/// shell version this replaces; the same class of bug applies here.
pub struct SupervisedService<'a> {
    pub name: &'a str,
    pub description: &'a str,
    pub exec_cmd: &'a str,
    /// Another unit/service name to order after, or `None`.
    pub after: Option<&'a str>,
    pub env: &'a [(&'a str, &'a str)],
    /// systemd `LimitSTACK=` value (e.g. `"infinity"`), or `None` for the
    /// system default (typically 8MB). Issue #528: an unoptimized debug
    /// build's per-task stack frame for a generic-heavy async state
    /// machine can exceed that default under real memory pressure --
    /// confirmed live, `nodecontroller` specifically, reproducibly
    /// segfaulting on startup ("starting all controllers" spawns many
    /// tokio tasks at once) under the system default stack limit, and
    /// running cleanly with an unlimited one. Only meaningful for the
    /// systemd tier; OpenRC/fallback installs ignore it (no equivalent
    /// stack-limit-only control at that layer here).
    pub limit_stack: Option<&'a str>,
}

pub fn install(cfg: &Config, svc: &SupervisedService) -> Result<()> {
    if crate::pkg::command_exists("systemctl") {
        return install_systemd(svc);
    }
    if crate::pkg::command_exists("rc-service") && crate::pkg::command_exists("rc-update") {
        return install_openrc(svc);
    }
    install_fallback(cfg, svc)
}

/// Undoes `install`, best-effort across all three tiers since a given
/// machine's install isn't tracked by which tier it used. Every step is
/// independently best-effort (not short-circuited on the first failure) --
/// `service-mgr.sh`'s own comment on `remove_supervised_service` explains
/// why: a bare failing step under `set -e` silently aborted the rest of an
/// uninstall in this project's history once already.
pub fn remove(cfg: &Config, name: &str) {
    if crate::pkg::command_exists("systemctl") {
        let _ = std::process::Command::new("systemctl").args(["disable", "--now", &format!("{name}.service")]).status();
        let _ = std::fs::remove_file(format!("/etc/systemd/system/{name}.service"));
        let _ = std::process::Command::new("systemctl").arg("daemon-reload").status();
    }
    if crate::pkg::command_exists("rc-update") {
        let _ = std::process::Command::new("rc-service").args([name, "stop"]).status();
        let _ = std::process::Command::new("rc-update").args(["del", name, "default"]).status();
        let _ = std::fs::remove_file(format!("/etc/init.d/{name}"));
    }
    let supervisor = cfg.work_dir().join(format!("{name}-supervisor.sh"));
    if crate::pkg::command_exists("crontab") {
        let _ = remove_crontab_entry(&supervisor);
    }
    let _ = std::process::Command::new("pkill").args(["-f", &supervisor.to_string_lossy()]).status();
    let _ = std::fs::remove_file(&supervisor);
}

fn env_lines_systemd(env: &[(&str, &str)]) -> String {
    env.iter().map(|(k, v)| format!("Environment={k}={v}\n")).collect()
}

fn env_lines_shell(env: &[(&str, &str)]) -> String {
    env.iter().map(|(k, v)| format!("export {k}={v}\n")).collect()
}

/// Builds the `.service` unit's contents. Separate from `install_systemd`
/// so the generated text is unit-testable without a real systemd on the
/// test host.
fn systemd_unit(svc: &SupervisedService) -> String {
    let after = svc.after.map(|a| format!(" {a}")).unwrap_or_default();
    let limit_stack = svc.limit_stack.map(|v| format!("LimitSTACK={v}\n")).unwrap_or_default();
    format!(
        "[Unit]\n\
         Description={desc}\n\
         After=network-online.target{after}\n\
         Wants=network-online.target{after}\n\
         \n\
         [Service]\n\
         Type=simple\n\
         ExecStart=/bin/sh -c '{exec_cmd}'\n\
         Restart=always\n\
         RestartSec=5s\n\
         {limit_stack}\
         {envs}\
         [Install]\n\
         WantedBy=multi-user.target\n",
        desc = svc.description,
        exec_cmd = svc.exec_cmd,
        envs = env_lines_systemd(svc.env),
    )
}

fn install_systemd(svc: &SupervisedService) -> Result<()> {
    tracing::info!(name = svc.name, "installing as a systemd service (Restart=always, enabled on boot)");
    let path = format!("/etc/systemd/system/{}.service", svc.name);
    std::fs::write(&path, systemd_unit(svc)).with_context(|| format!("writing {path}"))?;
    run_ok("systemctl", &["daemon-reload"])?;
    run_ok("systemctl", &["enable", &format!("{}.service", svc.name)])?;
    run_ok("systemctl", &["restart", &format!("{}.service", svc.name)])?;
    std::thread::sleep(std::time::Duration::from_secs(2));
    if !run_ok("systemctl", &["is-active", "--quiet", &format!("{}.service", svc.name)]).unwrap_or(false) {
        tracing::warn!(name = svc.name, "service didn't come up cleanly -- check: journalctl -u {} -n 50", svc.name);
    }
    Ok(())
}

/// Builds the OpenRC init script's contents. `after` becomes a `depend()`
/// clause on the unit name with any `.service` suffix stripped, matching
/// OpenRC's own naming (no `.service` there).
fn openrc_script(svc: &SupervisedService) -> String {
    let depend_after = svc.after.map(|a| format!("    after {}\n", a.trim_end_matches(".service"))).unwrap_or_default();
    format!(
        "#!/sbin/openrc-run\n\
         description=\"{desc}\"\n\
         \n\
         {envs}\
         supervisor=\"supervise-daemon\"\n\
         command=\"/bin/sh\"\n\
         command_args=\"-c '{exec_cmd}'\"\n\
         respawn_max=0\n\
         respawn_delay=5\n\
         \n\
         depend() {{\n\
         \x20   need net\n\
         {depend_after}}}\n",
        desc = svc.description,
        exec_cmd = svc.exec_cmd,
        envs = env_lines_shell(svc.env),
    )
}

fn install_openrc(svc: &SupervisedService) -> Result<()> {
    tracing::info!(name = svc.name, "installing as an OpenRC service (supervised, auto-restart, added to boot)");
    let path = format!("/etc/init.d/{}", svc.name);
    std::fs::write(&path, openrc_script(svc)).with_context(|| format!("writing {path}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).with_context(|| format!("making {path} executable"))?;
    }
    let _ = std::process::Command::new("rc-update").args(["add", svc.name, "default"]).status();
    if !run_ok("rc-service", &[svc.name, "restart"])? {
        run_ok("rc-service", &[svc.name, "start"])?;
    }
    std::thread::sleep(std::time::Duration::from_secs(2));
    let status_ok = std::process::Command::new("rc-service")
        .args([svc.name, "status"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).to_lowercase().contains("started"))
        .unwrap_or(false);
    if !status_ok {
        tracing::warn!(name = svc.name, "OpenRC service didn't come up cleanly -- check: rc-service {} status", svc.name);
    }
    Ok(())
}

/// Builds the self-restarting supervisor script's contents -- the
/// last-resort tier for a host with neither systemd nor OpenRC.
fn fallback_script(svc: &SupervisedService) -> String {
    format!(
        "#!/usr/bin/env bash\n\
         {envs}\
         while true; do\n\
         \x20   {exec_cmd}\n\
         \x20   sleep 5\n\
         done\n",
        envs = env_lines_shell(svc.env),
        exec_cmd = svc.exec_cmd,
    )
}

fn install_fallback(cfg: &Config, svc: &SupervisedService) -> Result<()> {
    tracing::warn!(
        name = svc.name,
        "no systemd or OpenRC on this system -- falling back to a self-restarting background \
         loop. Not a real service; set up this system's actual init/service manager to run \
         '{}' persistently when you can.",
        svc.exec_cmd
    );
    let work_dir = cfg.work_dir();
    std::fs::create_dir_all(&work_dir).context("creating work dir")?;
    let supervisor = work_dir.join(format!("{}-supervisor.sh", svc.name));
    std::fs::write(&supervisor, fallback_script(svc)).with_context(|| format!("writing {}", supervisor.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&supervisor, std::fs::Permissions::from_mode(0o755))
            .context("making the supervisor script executable")?;
    }

    let _ = std::process::Command::new("pkill")
        .args(["-f", &supervisor.to_string_lossy()])
        .status();

    let pid_path = work_dir.join(format!("{}.pid", svc.name));
    if let Ok(pid) = std::fs::read_to_string(&pid_path) {
        if let Ok(pid) = pid.trim().parse::<i32>() {
            let _ = std::process::Command::new("kill").arg(pid.to_string()).status();
        }
    }

    let log_path = cfg.log_dir().join(format!("{}.log", svc.name));
    std::fs::create_dir_all(cfg.log_dir()).context("creating log dir")?;
    let log_file = std::fs::File::create(&log_path).with_context(|| format!("creating {}", log_path.display()))?;
    let child = std::process::Command::new(&supervisor)
        .stdin(std::process::Stdio::null())
        .stdout(log_file.try_clone().context("cloning log file handle")?)
        .stderr(log_file)
        .spawn()
        .with_context(|| format!("spawning {}", supervisor.display()))?;
    std::fs::write(work_dir.join(format!("{}.pid", svc.name)), child.id().to_string())
        .context("writing pid file")?;
    std::thread::sleep(std::time::Duration::from_secs(2));

    if crate::pkg::command_exists("crontab") {
        match add_reboot_crontab_entry(&supervisor, &log_path) {
            Ok(()) => tracing::info!(name = svc.name, "added a cron @reboot entry, so this also restarts after a reboot"),
            Err(e) => tracing::warn!(name = svc.name, error = ?e, "couldn't add a cron @reboot entry -- this will NOT survive a reboot on this system"),
        }
    } else {
        tracing::warn!(name = svc.name, "no cron either -- this will NOT survive a reboot on this system");
    }
    Ok(())
}

fn current_crontab() -> String {
    std::process::Command::new("crontab").arg("-l").output().map(|o| String::from_utf8_lossy(&o.stdout).to_string()).unwrap_or_default()
}

fn add_reboot_crontab_entry(supervisor: &std::path::Path, log_path: &std::path::Path) -> Result<()> {
    let marker = supervisor.to_string_lossy().to_string();
    let mut lines: Vec<String> = current_crontab().lines().filter(|l| !l.contains(&marker)).map(str::to_string).collect();
    lines.push(format!("@reboot {} >>{} 2>&1 &", supervisor.display(), log_path.display()));
    write_crontab(&lines.join("\n"))
}

fn remove_crontab_entry(supervisor: &std::path::Path) -> Result<()> {
    let marker = supervisor.to_string_lossy().to_string();
    let lines: Vec<String> = current_crontab().lines().filter(|l| !l.contains(&marker)).map(str::to_string).collect();
    write_crontab(&lines.join("\n"))
}

fn write_crontab(contents: &str) -> Result<()> {
    use std::io::Write;
    let mut child = std::process::Command::new("crontab")
        .arg("-")
        .stdin(std::process::Stdio::piped())
        .spawn()
        .context("spawning crontab -")?;
    child.stdin.take().expect("stdin was piped").write_all(contents.as_bytes()).context("writing crontab input")?;
    if !contents.is_empty() {
        // crontab -l always ends without a trailing newline issue in
        // practice, but be defensive: ensure the file itself ends in one.
    }
    let status = child.wait().context("waiting for crontab -")?;
    anyhow::ensure!(status.success(), "crontab - failed");
    Ok(())
}

fn run_ok(program: &str, args: &[&str]) -> Result<bool> {
    Ok(std::process::Command::new(program).args(args).status().with_context(|| format!("running {program} {}", args.join(" ")))?.success())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_svc() -> SupervisedService<'static> {
        SupervisedService {
            name: "flanneld",
            description: "flanneld -- CNI overlay network daemon for not-k8s",
            exec_cmd: "/var/lib/nodebootstrap/toolchain/bin/flanneld -kubeconfig-file=/etc/nodebootstrap/admin.kubeconfig",
            after: Some("nodestore.service"),
            env: &[("NODE_NAME", "test-node"), ("IP_FAMILY", "ipv4")],
            limit_stack: None,
        }
    }

    #[test]
    fn systemd_unit_uses_absolute_exec_path_and_restart_always() {
        let unit = systemd_unit(&test_svc());
        assert!(unit.contains("ExecStart=/bin/sh -c '/var/lib/nodebootstrap/toolchain/bin/flanneld"));
        assert!(unit.contains("Restart=always"));
        assert!(unit.contains("After=network-online.target nodestore.service"));
        assert!(unit.contains("Environment=NODE_NAME=test-node"));
        assert!(unit.contains("Environment=IP_FAMILY=ipv4"));
    }

    #[test]
    fn limit_stack_is_absent_by_default() {
        assert!(!systemd_unit(&test_svc()).contains("LimitSTACK"));
    }

    #[test]
    fn limit_stack_is_emitted_when_set() {
        // Issue #528.
        let mut svc = test_svc();
        svc.limit_stack = Some("infinity");
        assert!(systemd_unit(&svc).contains("LimitSTACK=infinity\n"));
    }

    #[test]
    fn openrc_script_strips_service_suffix_from_depend() {
        let script = openrc_script(&test_svc());
        assert!(script.starts_with("#!/sbin/openrc-run\n"));
        assert!(script.contains("after nodestore\n"), "script was:\n{script}");
        assert!(!script.contains("after nodestore.service"));
        assert!(script.contains("export NODE_NAME=test-node"));
        assert!(script.contains("need net"));
    }

    #[test]
    fn fallback_script_loops_forever_with_a_restart_delay() {
        let script = fallback_script(&test_svc());
        assert!(script.contains("while true; do"));
        assert!(script.contains("sleep 5"));
        assert!(script.contains("export IP_FAMILY=ipv4"));
    }
}
