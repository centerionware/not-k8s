//! containerd + runc install/verify — replaces
//! `deploy/lib/container-runtime.sh`.
//!
//! Ports cgroup-mount verification, the presence/package-manager/official-
//! prebuilt/from-source tiers (the last needs `toolchain::ensure_go`),
//! `config.toml` generation and the three patches this project depends on
//! (nested-container native snapshotter, CDI device injection, the
//! CRI-plugin `disabled_plugins` strip from the "Known CI gotcha" in
//! `CLAUDE.md`), and starting containerd -- via its own distro-packaged
//! systemd unit if one exists, `service_mgr.rs` (systemd -> OpenRC ->
//! fallback loop) otherwise.

use anyhow::{Context, Result};

use crate::config::Config;
use crate::pkg::{fetch_url, pkg_install, PkgNames};

const CONFIG_PATH: &str = "/etc/containerd/config.toml";
const SOCKET_PATH: &str = "/run/containerd/containerd.sock";

pub fn run() -> Result<()> {
    run_with(&Config::from_env()?)
}

pub fn run_with(cfg: &Config) -> Result<()> {
    if cfg.skip_containerd {
        tracing::info!("skipping containerd setup (NODEBOOTSTRAP_SKIP_CONTAINERD)");
        return Ok(());
    }
    ensure_cgroups_mounted();
    ensure_binaries(cfg)?;
    let wrote_fresh_config = ensure_config()?;
    let removed_disabled_cri = strip_disabled_cri()?;
    ensure_running(cfg, wrote_fresh_config || removed_disabled_cri)?;
    Ok(())
}


/// See `container-runtime.sh`'s `ensure_cgroups_mounted` header comment for
/// the full story (Alpine/OpenRC doesn't mount `/sys/fs/cgroup` by default,
/// which surfaces as a confusing runc error much later, not here). This
/// port only does the check-and-warn half; enabling OpenRC's `cgroups`
/// service is part of the OpenRC service-writer gap noted in this module's
/// doc comment.
fn ensure_cgroups_mounted() {
    let mounted = std::fs::read_to_string("/proc/mounts")
        .map(|m| m.lines().any(|l| l.split_whitespace().nth(1) == Some("/sys/fs/cgroup")))
        .unwrap_or(false);
    if !mounted {
        tracing::warn!(
            "nothing is mounted at /sys/fs/cgroup -- container creation will fail in runc with \
             'no cgroup mount found in mountinfo'"
        );
    }
}

fn ensure_binaries(cfg: &Config) -> Result<()> {
    if crate::pkg::command_exists("containerd") && crate::pkg::command_exists("runc") {
        tracing::info!("containerd + runc already present");
        return Ok(());
    }
    let names =
        PkgNames { apt: "containerd runc", dnf: "containerd runc", pacman: "containerd runc", apk: "containerd runc", zypper: "containerd runc", xbps: "containerd runc" };
    let _ = pkg_install("containerd/runc", &names);
    if crate::pkg::command_exists("containerd") && crate::pkg::command_exists("runc") {
        return Ok(());
    }
    fetch_prebuilt(cfg)?;
    if crate::pkg::command_exists("containerd") && crate::pkg::command_exists("runc") {
        return Ok(());
    }
    build_from_source(cfg)
}

/// Deepest fallback: build both from source. Needs Go and `git`.
fn build_from_source(cfg: &Config) -> Result<()> {
    tracing::warn!("no prebuilt containerd/runc for this arch -- building both from source (needs Go)");
    crate::toolchain::ensure_go(cfg).context("containerd/runc's from-source build needs Go")?;
    if !crate::pkg::command_exists("git") {
        let names = PkgNames { apt: "git", dnf: "git", pacman: "git", apk: "git", zypper: "git", xbps: "git" };
        let _ = pkg_install("git", &names);
    }
    anyhow::ensure!(crate::pkg::command_exists("git"), "need git to fetch containerd/runc source and couldn't get it");

    let src_dir = cfg.src_dir();
    let toolchain_bin = cfg.toolchain_dir().join("bin");
    std::fs::create_dir_all(&toolchain_bin).context("creating toolchain bin dir")?;

    if !crate::pkg::command_exists("runc") {
        const RUNC_TAG: &str = "v1.1.14";
        let runc_dir = src_dir.join("runc");
        if !runc_dir.is_dir() {
            git_clone_shallow("https://github.com/opencontainers/runc.git", RUNC_TAG, &runc_dir)?;
        }
        run_in("make", &[], &runc_dir)?;
        install_binary(&runc_dir.join("runc"), &toolchain_bin.join("runc"))?;
        crate::toolchain::put_toolchain_bin_on_path(cfg);
    }
    if !crate::pkg::command_exists("containerd") {
        const CONTAINERD_TAG: &str = "v1.7.23";
        let containerd_dir = src_dir.join("containerd");
        if !containerd_dir.is_dir() {
            git_clone_shallow("https://github.com/containerd/containerd.git", CONTAINERD_TAG, &containerd_dir)?;
        }
        run_in("make", &[], &containerd_dir)?;
        install_binary(&containerd_dir.join("bin/containerd"), &toolchain_bin.join("containerd"))?;
        let shim = containerd_dir.join("bin/containerd-shim-runc-v2");
        if shim.exists() {
            install_binary(&shim, &toolchain_bin.join("containerd-shim-runc-v2"))?;
        }
        crate::toolchain::put_toolchain_bin_on_path(cfg);
    }

    anyhow::ensure!(
        crate::pkg::command_exists("containerd") && crate::pkg::command_exists("runc"),
        "containerd/runc source build did not produce usable binaries"
    );
    Ok(())
}

fn git_clone_shallow(url: &str, tag: &str, dest: &std::path::Path) -> Result<()> {
    let status = std::process::Command::new("git")
        .args(["clone", "--depth", "1", "--branch", tag, url])
        .arg(dest)
        .status()
        .with_context(|| format!("cloning {url}"))?;
    anyhow::ensure!(status.success(), "git clone {url} failed");
    Ok(())
}

fn run_in(program: &str, args: &[&str], cwd: &std::path::Path) -> Result<()> {
    let status = std::process::Command::new(program)
        .args(args)
        .current_dir(cwd)
        .status()
        .with_context(|| format!("running {program} {} in {}", args.join(" "), cwd.display()))?;
    anyhow::ensure!(status.success(), "{program} {} failed in {}", args.join(" "), cwd.display());
    Ok(())
}

fn install_binary(src: &std::path::Path, dest: &std::path::Path) -> Result<()> {
    std::fs::copy(src, dest).with_context(|| format!("copying {} to {}", src.display(), dest.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dest, std::fs::Permissions::from_mode(0o755))
            .with_context(|| format!("making {} executable", dest.display()))?;
    }
    Ok(())
}

fn fetch_prebuilt(cfg: &Config) -> Result<()> {
    let arch = cfg.arch();
    let (containerd_arch, runc_arch): (Option<&str>, Option<&str>) = match arch.as_str() {
        "x86_64" => (Some("amd64"), Some("amd64")),
        "aarch64" => (Some("arm64"), Some("arm64")),
        "armv7l" => (None, Some("armhf")),
        "ppc64le" => (Some("ppc64le"), Some("ppc64le")),
        "s390x" => (Some("s390x"), Some("s390x")),
        "riscv64" => (None, Some("riscv64")),
        _ => (None, None),
    };
    let toolchain_dir = cfg.toolchain_dir();
    let src_dir = cfg.src_dir();
    std::fs::create_dir_all(&src_dir).context("creating scratch dir")?;
    std::fs::create_dir_all(toolchain_dir.join("bin")).context("creating toolchain bin dir")?;

    if let Some(c_arch) = containerd_arch {
        if !crate::pkg::command_exists("containerd") {
            const VERSION: &str = "1.7.23";
            let tarball = src_dir.join("containerd.tar.gz");
            tracing::info!(arch = c_arch, "fetching official containerd release");
            if fetch_url(
                &format!(
                    "https://github.com/containerd/containerd/releases/download/v{VERSION}/containerd-{VERSION}-linux-{c_arch}.tar.gz"
                ),
                &tarball,
            )
            .is_ok()
            {
                let _ = std::process::Command::new("tar")
                    .args(["xzf"])
                    .arg(&tarball)
                    .arg("-C")
                    .arg(&toolchain_dir)
                    .status();
            }
        }
    }
    if let Some(r_arch) = runc_arch {
        if !crate::pkg::command_exists("runc") {
            const VERSION: &str = "1.1.14";
            let dest = toolchain_dir.join("bin/runc");
            tracing::info!(arch = r_arch, "fetching official runc release");
            if fetch_url(
                &format!("https://github.com/opencontainers/runc/releases/download/v{VERSION}/runc.{r_arch}"),
                &dest,
            )
            .is_ok()
            {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    let _ = std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(0o755));
                }
            }
        }
    }
    Ok(())
}

/// Writes `containerd config default` if no config exists yet, then applies
/// the nested-environment snapshotter and CDI patches this project always
/// needs regardless of where the config came from. Returns whether a fresh
/// config was written (so the caller knows to restart an already-running
/// containerd).
fn ensure_config() -> Result<bool> {
    std::fs::create_dir_all("/etc/containerd").context("creating /etc/containerd")?;
    let wrote_fresh = !std::path::Path::new(CONFIG_PATH).exists();
    if wrote_fresh {
        let out = std::process::Command::new("containerd")
            .args(["config", "default"])
            .output()
            .context("running containerd config default")?;
        anyhow::ensure!(out.status.success(), "containerd config default failed");
        std::fs::write(CONFIG_PATH, out.stdout).context("writing config.toml")?;
    }

    let mut config = std::fs::read_to_string(CONFIG_PATH).context("reading config.toml")?;

    // Nested container environment (this pipeline running inside a
    // container itself, e.g. CI) can't mount overlayfs -- see
    // container-runtime.sh's comment.
    let nested = std::fs::read_to_string("/proc/1/cgroup")
        .map(|c| c.contains("docker") || c.contains("containerd") || c.contains("kubepods"))
        .unwrap_or(false);
    if nested && config.contains(r#"snapshotter = "overlayfs""#) {
        tracing::info!("nested container environment detected -- using the native snapshotter");
        config = config.replace(r#"snapshotter = "overlayfs""#, r#"snapshotter = "native""#);
    }

    // CDI device injection, off by default -- see this module's doc
    // comment and container-runtime.sh's own for why not-k8s's DRA support
    // needs this on.
    if !config.lines().any(|l| l.trim_start() == "enable_cdi = true") {
        config = config.replace("enable_cdi = false", "enable_cdi = true");
    }

    std::fs::write(CONFIG_PATH, config).context("writing patched config.toml")?;
    Ok(wrote_fresh)
}

/// The "Known CI gotcha" fix from `CLAUDE.md`: strip `"cri"` out of
/// `disabled_plugins` unconditionally (not just on a freshly-generated
/// config), because a containerd that came from somewhere else entirely
/// (Docker's own package, on `ubuntu-latest` runners) ships one with CRI
/// disabled. Returns whether anything was actually changed.
fn strip_disabled_cri() -> Result<bool> {
    let config = std::fs::read_to_string(CONFIG_PATH).context("reading config.toml")?;
    if !config.lines().any(|l| {
        let l = l.trim_start();
        l.starts_with("disabled_plugins") && l.contains("\"cri\"")
    }) {
        return Ok(false);
    }
    tracing::info!("containerd's config.toml disables the CRI plugin (likely a Docker-managed install) -- enabling it");
    let patched: String = config
        .lines()
        .map(|l| {
            if l.trim_start().starts_with("disabled_plugins") {
                l.replace("\"cri\", ", "").replace("\"cri\",", "").replace("\"cri\"", "")
            } else {
                l.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(CONFIG_PATH, patched).context("writing CRI-enabled config.toml")?;
    Ok(true)
}

fn containerd_running() -> bool {
    std::path::Path::new(SOCKET_PATH).exists()
}

/// A distro-packaged containerd almost certainly already shipped its own
/// systemd unit (likely better-tuned -- cgroup delegation, OOM score --
/// than anything worth generating) -- use that instead of
/// `service_mgr.rs`'s generic writer, same as `container-runtime.sh`'s own
/// comment on this. Only meaningful on systemd; OpenRC/fallback hosts never
/// have a pre-existing containerd unit for this crate to have not written
/// itself, so they always go through `service_mgr::install`.
fn has_existing_systemd_unit() -> bool {
    if !crate::pkg::command_exists("systemctl") {
        return false;
    }
    std::process::Command::new("systemctl")
        .args(["list-unit-files", "containerd.service"])
        .output()
        .map(|output| {
            output.status.success()
                && String::from_utf8_lossy(&output.stdout)
                    .lines()
                    .any(|line| line.split_whitespace().next() == Some("containerd.service"))
        })
        .unwrap_or(false)
}

fn ensure_running(cfg: &Config, needs_restart: bool) -> Result<()> {
    if containerd_running() && crate::pkg::command_exists("systemctl") && has_existing_systemd_unit() {
        tracing::info!(config_changed = needs_restart, "restarting containerd during bootstrap update");
        run_systemctl(&["restart", "containerd.service"])?;
    } else if !containerd_running() {
        if has_existing_systemd_unit() {
            tracing::info!("containerd has an existing systemd unit -- enabling and starting it");
            run_systemctl(&["enable", "--now", "containerd.service"])?;
        } else {
            let containerd_bin = std::process::Command::new("sh")
                .arg("-c")
                .arg("command -v containerd")
                .output()
                .ok()
                .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
                .filter(|s| !s.is_empty())
                .context("resolving containerd's absolute path")?;
            crate::service_mgr::install(
                cfg,
                &crate::service_mgr::SupervisedService {
                    name: "containerd",
                    description: "containerd container runtime (installed by not-k8s)",
                    exec_cmd: &format!("{containerd_bin} --config {CONFIG_PATH}"),
                    after: None,
                    env: &[],
                },
            )?;
        }
    }

    for _ in 0..15 {
        if containerd_running() {
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_secs(1));
    }
    anyhow::bail!("containerd did not create its socket at {SOCKET_PATH} -- check journalctl -u containerd")
}

fn run_systemctl(args: &[&str]) -> Result<()> {
    let status = std::process::Command::new("systemctl")
        .args(args)
        .status()
        .with_context(|| format!("running systemctl {}", args.join(" ")))?;
    anyhow::ensure!(status.success(), "systemctl {} failed", args.join(" "));
    Ok(())
}
