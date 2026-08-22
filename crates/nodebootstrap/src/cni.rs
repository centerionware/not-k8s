//! CNI setup — replaces `deploy/lib/cni.sh` + `deploy/run-flanneld.sh`.
//!
//! **Scope cut, deliberate:** ports the plugin-binary/config-file half
//! faithfully (`ensure_cni_base_plugins`, `ensure_flannel_binaries`,
//! `write_flannel_cni_conf` -- package manager -> official prebuilt tiers,
//! matching `toolchain.rs`/`containerd.rs`'s same two-tier cut), and now
//! starts `flanneld` itself via `service_mgr.rs`. The net-conf.json
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

    anyhow::bail!(
        "no CNI base plugins for arch '{arch}' after the package manager and official-prebuilt \
         tiers -- the from-source fallback (containernetworking/plugins' build_linux.sh, Go-\
         toolchain-gated) is not yet ported here"
    )
}

fn ensure_flannel_binaries(cfg: &Config) -> Result<()> {
    let toolchain_bin = cfg.toolchain_dir().join("bin");
    std::fs::create_dir_all(&toolchain_bin).context("creating toolchain bin dir")?;

    if !crate::pkg::command_exists("flanneld") {
        let names = PkgNames { apt: "flannel", dnf: "flannel", pacman: "flannel", apk: "flannel", zypper: "flannel", xbps: "flannel" };
        let _ = pkg_install("flannel", &names);
    }

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
    anyhow::ensure!(
        crate::pkg::command_exists("flanneld"),
        "no flanneld for arch '{arch}' after the package manager and official-prebuilt tiers -- \
         the from-source fallback is not yet ported here"
    );

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
    anyhow::ensure!(
        is_executable(&cni_flannel),
        "could not obtain the flannel CNI plugin binary for arch '{arch}' -- the from-source \
         fallback is not yet ported here"
    );
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
