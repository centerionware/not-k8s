//! Package-manager detection/install and URL fetch shared by `toolchain.rs`,
//! `containerd.rs`, and `cni.rs` -- replaces the corresponding parts of
//! `deploy/lib/common.sh` (`detect_pkg_mgr`/`pkg_install`/`fetch`).
//!
//! **Scope cut, deliberate:** ports the primary path (detect a package
//! manager, `install`). Does **not** yet port `common.sh`'s apt
//! alternate-mirror retry-on-timeout logic (`_apt_run`/
//! `_apt_alternate_sources`) or the `pkg_installs.log` uninstall-tracking
//! file `deploy/lib/uninstall.sh` reads -- both are real and both are next,
//! not dropped.
//!
//! `fetch_url` is a real Rust HTTP client (`ureq`, rustls-backed), not a
//! `curl`/`wget` subprocess -- unlike `rbac.rs`/`manifests.rs` shelling out
//! to `kubectl` (a deliberate choice explained in those modules: `kubectl`
//! *is* the client, not a stand-in for one this crate could write itself),
//! a plain HTTPS GET is exactly what a Rust HTTP client is for.

use anyhow::{Context, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
    Ok(status.success())
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
