//! CNI setup — replaces `deploy/lib/cni.sh` + `deploy/run-flanneld.sh`.
//!
//! Ports the plugin-binary/config-file logic in full (`ensure_cni_base_
//! plugins`, `ensure_flannel_binaries`, `write_flannel_cni_conf` --
//! package manager -> official prebuilt -> from-source, matching
//! `toolchain.rs`/`containerd.rs`'s same tiers), and starts `flanneld`
//! itself via `service_mgr.rs`. The net-conf.json
//! generation + default-interface detection that has to happen on *every*
//! `flanneld` restart, not just at install time (a `Restart=always` unit
//! only re-runs its `ExecStart`, never this crate -- see
//! `vendor/run-flanneld.sh`'s own header comment for the real incident that
//! taught this) stays a **vendored shell wrapper** (`vendor/run-
//! flanneld.sh`, copied verbatim from `deploy/run-flanneld.sh`) that
//! `service_mgr.rs` points `ExecStart`/`command_args` at, rather than
//! reimplemented as Rust nodebootstrap itself would have to keep running
//! persistently to redo. Only reachable once `targets::run_with` has
//! started an apiserver `flanneld`'s kube-subnet-mgr mode can actually
//! reach -- `run_all()`'s ordering (`cni` after `targets`) is what
//! guarantees that.

use anyhow::{Context, Result};

use crate::config::Config;
use crate::pkg::{fetch_url, pkg_install, PkgNames};
use crate::service_mgr::{self, SupervisedService};

const CNI_BIN_DIR: &str = "/opt/cni/bin";
const CNI_CONF_DIR: &str = "/etc/cni/net.d";
const FLANNEL_SUBNET_ENV: &str = "/run/flannel/subnet.env";

/// Copied verbatim from `deploy/run-flanneld.sh` -- see that file and this
/// module's doc comment for why it stays a shell script rather than being
/// reimplemented in Rust. Vendored under this crate (not `include_str!`'d
/// from `deploy/` directly) so it survives `deploy/`'s shell libs being
/// deleted once Phase 1 fully cuts over (`docs/NODEBOOTSTRAP_PLAN.md`'s
/// phasing section).
const RUN_FLANNELD_SH: &str = include_str!("../vendor/run-flanneld.sh");

pub fn run() -> Result<()> {
    run_with(&Config::from_env()?)
}

pub fn run_with(cfg: &Config) -> Result<()> {
    let Some(provider) = &cfg.cni_provider else {
        tracing::info!("skipping CNI setup (NODEBOOTSTRAP_CNI=none) -- bring-your-own");
        return Ok(());
    };
    if provider != "flannel" {
        anyhow::bail!(
            "nodebootstrap only knows how to install 'flannel' itself; \
             NODEBOOTSTRAP_CNI={provider} means bring-your-own and skip this step \
             (set NODEBOOTSTRAP_CNI=none)"
        );
    }
    ensure_cni_base_plugins(cfg)?;
    ensure_flannel_binaries(cfg)?;
    write_flannel_cni_conf(std::path::Path::new(CNI_CONF_DIR))?;
    start_flanneld(cfg)
}

fn start_flanneld(cfg: &Config) -> Result<()> {
    let wrapper = cfg.work_dir().join("run-flanneld.sh");
    std::fs::create_dir_all(cfg.work_dir()).context("creating work dir")?;
    std::fs::write(&wrapper, RUN_FLANNELD_SH).with_context(|| format!("writing {}", wrapper.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755))
            .context("making run-flanneld.sh executable")?;
    }

    // Absolute path, not a bare command name -- systemd/OpenRC services get
    // a fresh, minimal PATH that won't include wherever ensure_flannel_
    // binaries put a fetched binary. Confirmed for real against exactly
    // this binary by cni.sh's own comment.
    let flanneld_bin = std::process::Command::new("sh")
        .arg("-c")
        .arg("command -v flanneld")
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
        .context("resolving flanneld's absolute path")?;

    let wrapper_exec = wrapper.to_string_lossy().to_string();
    let kubeconfig = cfg.kubeconfig_dir().join("admin.kubeconfig").to_string_lossy().to_string();
    let node_name = std::env::var("NODELET_NODE_NAME").unwrap_or_else(|_| {
        std::process::Command::new("uname").arg("-n").output().map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string()).unwrap_or_default()
    });
    let ip_family = cfg.ip_family();
    let ipv4_cidr = cfg.ipv4_cluster_cidr();
    let ipv6_cidr = cfg.ipv6_cluster_cidr();

    service_mgr::install(
        cfg,
        &SupervisedService {
            name: "flanneld",
            description: "flanneld -- CNI overlay network daemon for not-k8s",
            exec_cmd: &wrapper_exec,
            after: Some("kube-apiserver.service"),
            env: &[
                ("NODE_NAME", &node_name),
                ("FLANNELD_BIN", &flanneld_bin),
                ("KUBECONFIG", &kubeconfig),
                ("IP_FAMILY", &ip_family),
                ("IPV4_CLUSTER_CIDR", &ipv4_cidr),
                ("IPV6_CLUSTER_CIDR", &ipv6_cidr),
            ],
        },
    )
    .context("installing flanneld as a supervised service")
}

/// `service_mgr::install()` only proves that flanneld's supervisor started;
/// kube-subnet-mgr cannot write its lease until nodelet has registered the
/// Node object. Call this after nodelet starts, before reporting bootstrap
/// complete or allowing CRI pods to run.
pub fn wait_for_flannel_subnet(cfg: &Config) -> Result<()> {
    if cfg.cni_provider.as_deref() != Some("flannel") {
        return Ok(());
    }
    tracing::info!(path = FLANNEL_SUBNET_ENV, "waiting for flannel to allocate this node a pod subnet...");
    for _ in 0..15 {
        if std::fs::metadata(FLANNEL_SUBNET_ENV).map(|m| m.is_file() && m.len() > 0).unwrap_or(false) {
            tracing::info!(path = FLANNEL_SUBNET_ENV, "flannel pod subnet is ready");
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_secs(2));
    }
    anyhow::bail!(
        "flanneld never wrote {FLANNEL_SUBNET_ENV} within 30s -- check: journalctl -u flanneld -n 100"
    )
}

fn cni_go_arch(arch: &str) -> Option<&'static str> {
    Some(match arch {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        "armv7l" => "arm",
        "ppc64le" => "ppc64le",
        "s390x" => "s390x",
        "riscv64" => "riscv64",
        _ => return None,
    })
}

fn is_executable(path: &std::path::Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path).map(|m| m.permissions().mode() & 0o111 != 0).unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        path.exists()
    }
}

fn ensure_cni_base_plugins(cfg: &Config) -> Result<()> {
    let bin_dir = std::path::Path::new(CNI_BIN_DIR);
    if is_executable(&bin_dir.join("bridge")) && is_executable(&bin_dir.join("host-local")) {
        return Ok(());
    }
    std::fs::create_dir_all(bin_dir).context("creating CNI bin dir")?;

    let names = PkgNames {
        apt: "containernetworking-plugins",
        dnf: "containernetworking-plugins",
        pacman: "cni-plugins",
        apk: "cni-plugins",
        zypper: "containernetworking-plugins",
        xbps: "containernetworking-plugins",
    };
    if pkg_install("CNI plugins", &names)? {
        // Distro packages install to their own dir -- see cni.sh's comment
        // on why this checks the exact path, not `command -v bridge`
        // (which finds iproute2's unrelated `bridge` tool instead).
        for candidate in ["/usr/lib/cni", "/usr/libexec/cni"] {
            if is_executable(&std::path::Path::new(candidate).join("bridge")) {
                tracing::info!(dir = candidate, "using distro CNI plugins");
                return Ok(());
            }
        }
    }

    let arch = cfg.arch();
    if let Some(goarch) = cni_go_arch(&arch) {
        const VERSION: &str = "1.5.1";
        let tarball = cfg.src_dir().join("cni-plugins.tgz");
        std::fs::create_dir_all(cfg.src_dir()).context("creating scratch dir")?;
        tracing::info!(arch = goarch, "fetching official containernetworking/plugins release");
        if fetch_url(
            &format!(
                "https://github.com/containernetworking/plugins/releases/download/v{VERSION}/cni-plugins-linux-{goarch}-v{VERSION}.tgz"
            ),
            &tarball,
        )
        .is_ok()
        {
            let _ = std::process::Command::new("tar").args(["xzf"]).arg(&tarball).arg("-C").arg(bin_dir).status();
            if is_executable(&bin_dir.join("bridge")) {
                tracing::info!(dir = CNI_BIN_DIR, "CNI base plugins ready");
                return Ok(());
            }
        }
    }

    build_cni_base_plugins_from_source(cfg, bin_dir)
}

/// Deepest fallback: clone `containernetworking/plugins` and run its own
/// `build_linux.sh`. Needs Go.
fn build_cni_base_plugins_from_source(cfg: &Config, bin_dir: &std::path::Path) -> Result<()> {
    tracing::warn!("no prebuilt CNI plugins for this arch -- building from source (needs Go)");
    crate::toolchain::ensure_go(cfg).context("CNI base plugins' from-source build needs Go")?;
    if !crate::pkg::command_exists("git") {
        let names = PkgNames { apt: "git", dnf: "git", pacman: "git", apk: "git", zypper: "git", xbps: "git" };
        let _ = pkg_install("git", &names);
    }
    let src_dir = cfg.src_dir();
    let plugins_dir = src_dir.join("plugins");
    if !plugins_dir.is_dir() {
        let status = std::process::Command::new("git")
            .args(["clone", "--depth", "1", "--branch", "v1.5.1", "https://github.com/containernetworking/plugins.git"])
            .arg(&plugins_dir)
            .status()
            .context("cloning containernetworking/plugins")?;
        anyhow::ensure!(status.success(), "git clone containernetworking/plugins failed");
    }
    let status = std::process::Command::new("./build_linux.sh")
        .env("CGO_ENABLED", "0")
        .current_dir(&plugins_dir)
        .status()
        .context("running build_linux.sh")?;
    anyhow::ensure!(status.success(), "containernetworking/plugins' build_linux.sh failed");

    for entry in std::fs::read_dir(plugins_dir.join("bin")).context("reading plugins/bin")? {
        let entry = entry?;
        let dest = bin_dir.join(entry.file_name());
        std::fs::copy(entry.path(), &dest).with_context(|| format!("copying {}", dest.display()))?;
        chmod_executable(&dest);
    }
    anyhow::ensure!(is_executable(&bin_dir.join("bridge")), "CNI base plugin source build did not produce usable binaries");
    tracing::info!(dir = %bin_dir.display(), "CNI base plugins built from source");
    Ok(())
}

fn ensure_flannel_binaries(cfg: &Config) -> Result<()> {
    let toolchain_bin = cfg.toolchain_dir().join("bin");
    std::fs::create_dir_all(&toolchain_bin).context("creating toolchain bin dir")?;

    // flanneld shells out to iptables for --ip-masq -- see cni.sh's comment
    // on why this bites Alpine specifically (no iptables in a base image).
    if !crate::pkg::command_exists("iptables") {
        let names =
            PkgNames { apt: "iptables", dnf: "iptables", pacman: "iptables", apk: "iptables", zypper: "iptables", xbps: "iptables" };
        if !pkg_install("iptables", &names)? {
            tracing::warn!(
                "couldn't install iptables -- flanneld needs it for --ip-masq and will fail to \
                 set up masquerade rules"
            );
        }
    }

    let arch = cfg.arch();
    let goarch = cni_go_arch(&arch);
    if !crate::pkg::command_exists("flanneld") {
        if let Some(goarch) = goarch {
            const VERSION: &str = "0.25.6";
            let dest = toolchain_bin.join("flanneld");
            tracing::info!(arch = goarch, "fetching official flannel release");
            if fetch_url(
                &format!("https://github.com/flannel-io/flannel/releases/download/v{VERSION}/flanneld-{goarch}"),
                &dest,
            )
            .is_ok()
            {
                chmod_executable(&dest);
            }
        }
    }
    if !crate::pkg::command_exists("flanneld") {
        build_flanneld_from_source(cfg, &toolchain_bin)?;
    }
    anyhow::ensure!(crate::pkg::command_exists("flanneld"), "could not obtain a flanneld binary for arch '{arch}'");

    let cni_flannel = std::path::Path::new(CNI_BIN_DIR).join("flannel");
    if !is_executable(&cni_flannel) {
        if let Some(goarch) = goarch {
            let _ = fetch_url(
                &format!("https://github.com/flannel-io/cni-plugin/releases/download/v1.6.0-flannel1/flannel-{goarch}"),
                &cni_flannel,
            );
            chmod_executable(&cni_flannel);
        }
    }
    if !is_executable(&cni_flannel) {
        build_flannel_cni_plugin_from_source(cfg, goarch, &cni_flannel)?;
    }
    anyhow::ensure!(is_executable(&cni_flannel), "could not obtain the flannel CNI plugin binary for arch '{arch}'");
    Ok(())
}

/// Deepest fallback: clone `flannel-io/flannel` and build its flanneld entry
/// point with the verified musl C toolchain. Do not use upstream's Makefile
/// here: it forces `CGO_ENABLED=1` and lets the host compiler decide the C
/// runtime, which can link the supposedly static binary against glibc and
/// emits the exact runtime-dependency warning this bootstrap is designed to
/// avoid. Flannel's amd64 UDP backend does require cgo, so disabling it is
/// not a valid static-build strategy either.
fn build_flanneld_from_source(cfg: &Config, toolchain_bin: &std::path::Path) -> Result<()> {
    tracing::warn!("no prebuilt flanneld for this arch -- building from source (needs Go)");
    crate::toolchain::ensure_go(cfg).context("flanneld's from-source build needs Go")?;
    crate::toolchain::ensure_c_toolchain(cfg).context("flanneld's static from-source build needs a musl C compiler")?;
    let compiler = std::env::var("MUSL_C_COMPILER").context("musl compiler was not exported after C toolchain setup")?;
    let arch = cfg.arch();
    let goarch = cni_go_arch(&arch).context("no Go architecture mapping for flanneld's source build")?;
    if !crate::pkg::command_exists("git") {
        let names = PkgNames { apt: "git", dnf: "git", pacman: "git", apk: "git", zypper: "git", xbps: "git" };
        let _ = pkg_install("git", &names);
    }
    let flannel_dir = cfg.src_dir().join("flannel");
    if !flannel_dir.is_dir() {
        let status = std::process::Command::new("git")
            .args(["clone", "--depth", "1", "--branch", "v0.25.6", "https://github.com/flannel-io/flannel.git"])
            .arg(&flannel_dir)
            .status()
            .context("cloning flannel-io/flannel")?;
        anyhow::ensure!(status.success(), "git clone flannel-io/flannel failed");
    }
    let mut command = std::process::Command::new("go");
    let cgo_cflags = linux_uapi_cgo_flags(&compiler);
    command
        .args([
            "build",
            "-trimpath",
            "-o",
            "dist/flanneld",
            "-ldflags",
            "-s -w -X github.com/flannel-io/flannel/pkg/version.Version=v0.25.6 -linkmode external -extldflags '-static'",
            ".",
        ])
        .env("CGO_ENABLED", "1")
        .env("CC", &compiler)
        .env("CGO_CFLAGS", cgo_cflags)
        .env("GOOS", "linux")
        .env("GOARCH", goarch)
        .current_dir(&flannel_dir);
    if arch == "armv7l" {
        command.env("GOARM", "7");
    } else if arch == "armv6l" {
        command.env("GOARM", "6");
    }
    let status = command.status().context("building flanneld from source")?;
    anyhow::ensure!(status.success(), "flannel-io/flannel's from-source build failed");

    let dest = toolchain_bin.join("flanneld");
    std::fs::copy(flannel_dir.join("dist/flanneld"), &dest).with_context(|| format!("copying {}", dest.display()))?;
    chmod_executable(&dest);
    crate::toolchain::put_toolchain_bin_on_path(cfg);
    tracing::info!("flanneld built from source");
    Ok(())
}

/// Debian's `musl-gcc` wrapper intentionally limits its system include path
/// to musl's headers. Flannel's amd64 UDP backend also includes Linux UAPI
/// headers (`linux/ip.h` and its architecture-specific `asm/` includes), so
/// make those headers visible without putting glibc's standard headers,
/// libraries, or linker anywhere in the build. `-idirafter` is important:
/// musl's headers must remain ahead of the distro's general `/usr/include`
/// tree. The compiler still controls the C runtime and the Go link step
/// below still uses `-static`.
fn linux_uapi_cgo_flags(compiler: &str) -> String {
    let mut dirs = vec![std::path::PathBuf::from("/usr/include")];
    if let Ok(output) = std::process::Command::new(compiler).arg("-print-multiarch").output() {
        if output.status.success() {
            let multiarch = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !multiarch.is_empty() {
                dirs.push(std::path::PathBuf::from("/usr/include").join(multiarch));
            }
        }
    }
    dirs.into_iter()
        .filter(|dir| dir.is_dir())
        .map(|dir| format!("-idirafter {}", dir.display()))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Deepest fallback: clone `flannel-io/cni-plugin` and run its own
/// `build.sh`. Needs Go.
fn build_flannel_cni_plugin_from_source(cfg: &Config, goarch: Option<&str>, dest: &std::path::Path) -> Result<()> {
    tracing::warn!("no prebuilt flannel CNI plugin for this arch -- building from source (needs Go)");
    crate::toolchain::ensure_go(cfg).context("the flannel CNI plugin's from-source build needs Go")?;
    let cni_plugin_dir = cfg.src_dir().join("cni-plugin");
    if !cni_plugin_dir.is_dir() {
        let status = std::process::Command::new("git")
            .args(["clone", "--depth", "1", "--branch", "v1.6.0-flannel1", "https://github.com/flannel-io/cni-plugin.git"])
            .arg(&cni_plugin_dir)
            .status()
            .context("cloning flannel-io/cni-plugin")?;
        anyhow::ensure!(status.success(), "git clone flannel-io/cni-plugin failed");
    }
    let status = std::process::Command::new("./build.sh")
        .env("CGO_ENABLED", "0")
        .current_dir(&cni_plugin_dir)
        .status()
        .context("running build.sh")?;
    anyhow::ensure!(status.success(), "flannel-io/cni-plugin's build.sh failed");

    let goarch = goarch.unwrap_or("amd64");
    let built = cni_plugin_dir.join(format!("dist/flannel-{goarch}"));
    std::fs::copy(&built, dest).with_context(|| format!("copying {} to {}", built.display(), dest.display()))?;
    chmod_executable(dest);
    tracing::info!(path = %dest.display(), "flannel CNI plugin built from source");
    Ok(())
}

fn write_flannel_cni_conf(conf_dir: &std::path::Path) -> Result<()> {
    std::fs::create_dir_all(conf_dir).context("creating CNI conf dir")?;
    let path = conf_dir.join("10-flannel.conflist");
    if path.exists() {
        return Ok(());
    }
    let conflist = r#"{
  "name": "not-k8s-flannel",
  "cniVersion": "1.0.0",
  "plugins": [
    {
      "type": "flannel",
      "delegate": { "hairpinMode": true, "isDefaultGateway": true }
    },
    {
      "type": "portmap",
      "capabilities": { "portMappings": true }
    }
  ]
}
"#;
    std::fs::write(&path, conflist).with_context(|| format!("writing {}", path.display()))
}


fn chmod_executable(path: &std::path::Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch_dir(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("nodebootstrap-cni-test-{name}-{}", std::process::id()))
    }

    #[test]
    fn write_flannel_cni_conf_produces_valid_json() {
        let dir = scratch_dir("fresh");
        write_flannel_cni_conf(&dir).expect("write conf");
        let contents = std::fs::read_to_string(dir.join("10-flannel.conflist")).expect("read conf");
        let parsed: serde_json::Value = serde_json::from_str(&contents).expect("valid JSON");
        assert_eq!(parsed["plugins"][0]["type"], "flannel");
        assert_eq!(parsed["plugins"][1]["type"], "portmap");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn write_flannel_cni_conf_does_not_overwrite_an_existing_file() {
        let dir = scratch_dir("existing");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("10-flannel.conflist"), "{}").unwrap();
        write_flannel_cni_conf(&dir).expect("write conf");
        assert_eq!(std::fs::read_to_string(dir.join("10-flannel.conflist")).unwrap(), "{}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
