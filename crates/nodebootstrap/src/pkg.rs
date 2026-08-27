//! Package-manager detection/install and URL fetch shared by `toolchain.rs`,
//! `containerd.rs`, and `cni.rs` -- replaces the corresponding parts of
//! `deploy/lib/common.sh` (`detect_pkg_mgr`/`pkg_install`/`fetch`).
//!
//! The package manager log is deliberately ownership-scoped: uninstall only
//! removes packages this bootstrapper successfully asked the manager to
//! install, rather than guessing at every package with a familiar name.
//!
//! `fetch_url` is a real Rust HTTP client (`ureq`, rustls-backed), not a
//! `curl`/`wget` subprocess -- unlike `rbac.rs`/`manifests.rs` shelling out
//! to `kubectl` (a deliberate choice explained in those modules: `kubectl`
//! *is* the client, not a stand-in for one this crate could write itself),
//! a plain HTTPS GET is exactly what a Rust HTTP client is for.

use anyhow::{Context, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PkgMgr {
    Apt,
    Dnf,
    Yum,
    Pacman,
    Apk,
    Zypper,
    Xbps,
}

impl PkgMgr {
    /// Same probe order as `common.sh`'s `detect_pkg_mgr` (apt before dnf
    /// before yum, ...) -- order matters on a distro that happens to carry
    /// more than one manager's binary.
    pub fn detect() -> Option<Self> {
        let candidates = [
            ("apt-get", PkgMgr::Apt),
            ("dnf", PkgMgr::Dnf),
            ("yum", PkgMgr::Yum),
            ("pacman", PkgMgr::Pacman),
            ("apk", PkgMgr::Apk),
            ("zypper", PkgMgr::Zypper),
            ("xbps-install", PkgMgr::Xbps),
        ];
        candidates.into_iter().find(|(bin, _)| command_exists(bin)).map(|(_, mgr)| mgr)
    }

    fn name(self) -> &'static str {
        match self {
            Self::Apt => "apt",
            Self::Dnf => "dnf",
            Self::Yum => "yum",
            Self::Pacman => "pacman",
            Self::Apk => "apk",
            Self::Zypper => "zypper",
            Self::Xbps => "xbps",
        }
    }

    fn remove_command(self) -> (&'static str, [&'static str; 3]) {
        match self {
            Self::Apt => ("apt-get", ["remove", "-y", "-qq"]),
            Self::Dnf => ("dnf", ["remove", "-y", "-q"]),
            Self::Yum => ("yum", ["remove", "-y", "-q"]),
            Self::Pacman => ("pacman", ["-R", "--noconfirm", ""]),
            Self::Apk => ("apk", ["del", "--no-cache", ""]),
            Self::Zypper => ("zypper", ["--non-interactive", "remove", ""]),
            Self::Xbps => ("xbps-remove", ["-R", "", ""]),
        }
    }
}

/// One package's name under each manager this crate knows how to drive --
/// mirrors `pkg_install`'s 6 positional package-name arguments in
/// `common.sh` (that shell function's `yum` case reuses the `dnf` name, so
/// there's no separate `yum` field here either).
pub struct PkgNames<'a> {
    pub apt: &'a str,
    pub dnf: &'a str,
    pub pacman: &'a str,
    pub apk: &'a str,
    pub zypper: &'a str,
    pub xbps: &'a str,
}

pub fn command_exists(bin: &str) -> bool {
    std::process::Command::new("sh")
        .arg("-c")
        .arg(format!("command -v {bin}"))
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Installs `pkgs` via the detected manager, running through `sudo` when
/// not already root (same posture as `common.sh`'s `$SUDO` prefix).
/// Returns `Ok(false)`, not an error, when no supported manager was
/// found -- callers (`toolchain.rs`) fall through to their own prebuilt/
/// from-source tiers on that, exactly like the shell version's non-zero
/// return does.
pub fn pkg_install(display_name: &str, pkgs: &PkgNames) -> Result<bool> {
    let Some(mgr) = PkgMgr::detect() else {
        tracing::warn!("no supported package manager found, cannot install '{display_name}'");
        return Ok(false);
    };
    let (program, args): (&str, Vec<String>) = match mgr {
        PkgMgr::Apt => ("apt-get", split_pkgs("install", "-y", "-qq", pkgs.apt)),
        PkgMgr::Dnf => ("dnf", split_pkgs("install", "-y", "-q", pkgs.dnf)),
        PkgMgr::Yum => ("yum", split_pkgs("install", "-y", "-q", pkgs.dnf)),
        PkgMgr::Pacman => ("pacman", split_pkgs("-Sy", "--noconfirm", "--needed", pkgs.pacman)),
        PkgMgr::Apk => ("apk", split_pkgs("add", "--no-cache", "", pkgs.apk)),
        PkgMgr::Zypper => ("zypper", split_pkgs("--non-interactive", "install", "", pkgs.zypper)),
        PkgMgr::Xbps => ("xbps-install", split_pkgs("-Sy", "", "", pkgs.xbps)),
    };
    tracing::info!(pkg_mgr = ?mgr, "installing '{display_name}' via {program}");
    let status = sudo_command(program)
        .args(&args)
        .status()
        .with_context(|| format!("running {program} to install '{display_name}'"))?;
    if status.success() {
        record_package_install(mgr, selected_packages(mgr, pkgs))
            .with_context(|| format!("recording packages installed for '{display_name}'"))?;
    }
    Ok(status.success())
}

fn selected_packages<'a>(mgr: PkgMgr, pkgs: &'a PkgNames<'a>) -> &'a str {
    match mgr {
        PkgMgr::Apt => pkgs.apt,
        PkgMgr::Dnf | PkgMgr::Yum => pkgs.dnf,
        PkgMgr::Pacman => pkgs.pacman,
        PkgMgr::Apk => pkgs.apk,
        PkgMgr::Zypper => pkgs.zypper,
        PkgMgr::Xbps => pkgs.xbps,
    }
}

fn package_log_path() -> std::path::PathBuf {
    let state_dir = std::env::var("NODEBOOTSTRAP_STATE_DIR")
        .unwrap_or_else(|_| "/var/lib/nodebootstrap".to_string());
    std::path::PathBuf::from(state_dir).join("pkg_installs.log")
}

fn record_package_install(mgr: PkgMgr, packages: &str) -> Result<()> {
    let path = package_log_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating package tracking directory {}", parent.display()))?;
    }
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("opening package tracking log {}", path.display()))?;
    writeln!(file, "{}|{}", mgr.name(), packages.trim())
        .with_context(|| format!("writing package tracking log {}", path.display()))?;
    Ok(())
}

/// Remove only packages recorded by successful package-install calls. A
/// failure is retained in the log so a later uninstall can retry it.
pub fn remove_tracked_packages() -> Result<()> {
    let path = package_log_path();
    let contents = match std::fs::read_to_string(&path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error).with_context(|| format!("reading package tracking log {}", path.display())),
    };
    let mut removed = std::collections::HashSet::new();
    let mut failures = Vec::new();
    for line in contents.lines().filter(|line| !line.trim().is_empty()) {
        let Some((manager, packages)) = line.split_once('|') else {
            tracing::warn!(line, "ignoring malformed package tracking entry");
            continue;
        };
        let Some(mgr) = (match manager {
            "apt" => Some(PkgMgr::Apt),
            "dnf" => Some(PkgMgr::Dnf),
            "yum" => Some(PkgMgr::Yum),
            "pacman" => Some(PkgMgr::Pacman),
            "apk" => Some(PkgMgr::Apk),
            "zypper" => Some(PkgMgr::Zypper),
            "xbps" => Some(PkgMgr::Xbps),
            _ => None,
        }) else {
            tracing::warn!(manager, "ignoring unknown package manager in tracking log");
            continue;
        };
        let (program, command_args) = mgr.remove_command();
        for package in packages.split_whitespace() {
            if !removed.insert((mgr, package.to_string())) {
                continue;
            }
            let mut command = sudo_command(program);
            command.args(command_args.iter().copied().filter(|arg| !arg.is_empty()));
            command.arg(package);
            match command.status() {
                Ok(status) if status.success() => {
                    tracing::info!(package, pkg_mgr = ?mgr, "removed bootstrap-installed package");
                }
                Ok(status) => failures.push(format!("{program} {package} exited with {status}")),
                Err(error) => failures.push(format!("running {program} for {package}: {error}")),
            }
        }
    }
    if failures.is_empty() {
        let _ = std::fs::remove_file(&path);
        Ok(())
    } else {
        anyhow::bail!("some tracked packages could not be removed: {}", failures.join("; "))
    }
}

fn split_pkgs(cmd: &str, flag_a: &str, flag_b: &str, pkgs: &str) -> Vec<String> {
    let mut args: Vec<String> = vec![cmd.to_string()];
    for flag in [flag_a, flag_b] {
        if !flag.is_empty() {
            args.push(flag.to_string());
        }
    }
    args.extend(pkgs.split_whitespace().map(str::to_string));
    args
}

fn sudo_command(program: &str) -> std::process::Command {
    if is_root() {
        std::process::Command::new(program)
    } else {
        let mut cmd = std::process::Command::new("sudo");
        cmd.arg(program);
        cmd
    }
}

// No libc dependency for one euid check -- shell out to `id -u`, same cost
// as everything else in this module and one fewer crate in the tree.
fn is_root() -> bool {
    std::process::Command::new("id")
        .arg("-u")
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim() == "0")
        .unwrap_or(false)
}

/// A real HTTP client (`ureq`, rustls + webpki-roots), not a `curl`/`wget`
/// subprocess -- replaces `common.sh`'s `fetch()`, but no longer depends on
/// either tool being present on the host at all. Retries transient
/// failures up to 3 times (same retry count `curl --retry 3` gave the shell
/// version), with a short backoff between attempts.
pub fn fetch_url(url: &str, dest: &std::path::Path) -> Result<()> {
    const MAX_ATTEMPTS: u32 = 3;
    let mut attempt = 0;
    loop {
        attempt += 1;
        match ureq::get(url).call() {
            Ok(response) => {
                let mut reader = response.into_reader();
                let name = dest.file_name().and_then(|name| name.to_str()).unwrap_or("download");
                let temporary = dest.with_file_name(format!(".{name}.tmp-{}", std::process::id()));
                let _ = std::fs::remove_file(&temporary);
                let mut file =
                    std::fs::File::create(&temporary).with_context(|| format!("creating {}", temporary.display()))?;
                std::io::copy(&mut reader, &mut file)
                    .with_context(|| format!("writing response body to {}", temporary.display()))?;
                drop(file);
                std::fs::rename(&temporary, dest)
                    .with_context(|| format!("installing {} as {}", temporary.display(), dest.display()))?;
                return Ok(());
            }
            Err(e) if attempt < MAX_ATTEMPTS => {
                tracing::warn!(url, attempt, error = %e, "fetch attempt failed, retrying");
                std::thread::sleep(std::time::Duration::from_secs(1));
            }
            Err(e) => return Err(e).with_context(|| format!("fetching {url} (after {MAX_ATTEMPTS} attempts)")),
        }
    }
}
