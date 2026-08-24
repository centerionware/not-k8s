//! Library surface for `nodebootstrap`. See `docs/NODEBOOTSTRAP_PLAN.md` for
//! the module-to-shell-script mapping, the Phase 1 / Phase 2 split, and the
//! "Implementation status" table tracking which modules have real logic
//! versus a documented scope cut (most do now; each module's own doc
//! comment states its cut precisely). Land real logic group by group, each
//! its own branch/PR/e2e case per `CLAUDE.md`'s merge protocol.

pub mod config;
pub mod containerd;
pub mod cni;
pub mod components;
pub mod fetch;
pub mod kubeconfig;
pub mod manifests;
pub mod pkg;
pub mod pki;
pub mod rbac;
pub mod service_mgr;
pub mod service_reconciler;
pub mod services;
pub mod targets;
pub mod toolchain;

use anyhow::{bail, Context, Result};

/// Runs every phase in dependency order: toolchain -> containerd -> fetch
/// -> pki -> kubeconfig -> targets (install/start the apiserver) -> cni ->
/// service-reconciler -> manifests -> nodelet TLS/apiserver handoff -> rbac
/// and nodecontroller -> CNI readiness -> apiserver network endpoint refresh
/// -> nodescheduler and the remaining
/// replacement services.
/// This is what
/// `bootstrap-source.sh`/`bootstrap-release.sh` do today as one script;
/// here it's one function calling each module's `run_with()` in turn so any
/// individual step stays independently testable and independently
/// skippable (`config::Config`'s skip flags gate each call).
///
/// `services::ensure_nodestore` runs before `targets` -- `targets/
/// upstream.rs` orders `kube-apiserver.service` `After=nodestore.service`,
/// so nodestore has to actually be installed and enabled first.
/// `targets::run_with` itself runs after `pki`/`kubeconfig` (it needs the
/// minted PKI to start the apiserver trusting it) and before
/// `service_reconciler`/`manifests` (both need a reachable apiserver to
/// apply against). `cni` runs after the apiserver is up because flanneld's
/// kube-subnet-manager mode needs a live kubeconfig. Nodelet then generates
/// its serving certificate; `targets::enable_nodelet_proxy` may restart the
/// apiserver to trust that CA. The replacement scheduler/controller are
/// deliberately installed only after that first planned restart and after
/// the RBAC authorizer barrier. The scheduler must be running before the
/// later network endpoint refresh: otherwise CoreDNS stays Pending and no
/// CNI bridge is created for that refresh to discover. `nodecontroller` is
/// the deliberate exception to the CNI barrier: its node-ipam controller
/// allocates the PodCIDR that flanneld needs before it can lease this node a
/// subnet.
pub fn run_all() -> Result<()> {
    let cfg = config::Config::from_env()?;
    if matches!(cfg.source, config::Source::Compile) && !fetch::has_prebuilt() {
        toolchain::run_with(&cfg)?;
    }
    if cfg.with_cri && !cfg.skip_containerd {
        containerd::run_with(&cfg)?;
    }
    fetch::run_with(&cfg)?;
    pki::run_with(&cfg)?;
    kubeconfig::run_with(&cfg)?;

    if !cfg.skip_control_plane {
        services::ensure_nodestore(&cfg)?;
        targets::run_with(&cfg)?;
        if cfg.with_cri {
            cni::run_with(&cfg)?;
        }
        service_reconciler::run_with(&cfg)?;
        manifests::run_with(&cfg)?;
    }

    if !cfg.skip_nodelet {
        services::ensure_nodelet(&cfg)?;
        if !cfg.skip_control_plane && cfg.with_cri {
            targets::enable_nodelet_proxy(&cfg)?;
        }
    }
    if !cfg.skip_control_plane {
        rbac::run_with(&cfg)?;
        services::ensure_nodecontroller(&cfg)?;
        services::ensure_nodescheduler(&cfg)?;
        if !cfg.skip_nodelet && cfg.with_cri {
            cni::wait_for_flannel_subnet(&cfg)?;
            targets::refresh_network_advertise_address(&cfg)?;
        }
    }
    services::ensure_nodeproxy(&cfg)?;
    Ok(())
}

/// Runs the standalone command-line interface. The standalone release binary
/// is useful on a host with no checkout: it fetches the matching component
/// release assets. When run from a checkout, the default source is a local
/// release build after `toolchain::run_with` has made Cargo available.
pub fn run_args(args: &[String]) -> Result<()> {
    run_cli(args, false)
}

/// Runs the same CLI through the combined `notk8s` binary. The binary already
/// contains every runtime applet, so the default path stages the executable
/// itself instead of requiring a checkout or downloading a second copy.
pub fn run_embedded(args: &[String]) -> Result<()> {
    let explicit_source = args.iter().any(|arg| {
        matches!(arg.as_str(), "--from-source" | "--force-source-build" | "--release")
            || arg.starts_with("--source=")
            || arg.starts_with("--tag=")
            || arg == "--update"
            || arg == "--layout=split"
            || arg == "--layout=both"
    });
    if !explicit_source && std::env::var_os("NODEBOOTSTRAP_SOURCE").is_none() {
        let self_binary = std::env::current_exe().context("resolving the combined notk8s executable")?;
        std::env::set_var("NODEBOOTSTRAP_COMBINED_SELF", self_binary);
    }
    run_cli(args, true)
}

/// Returns the arguments for an embedded bootstrap invocation, if `argv` names
/// this applet directly or selects it through the combined binary. This check
/// belongs outside the async entrypoint: `parse_args` deliberately updates
/// environment variables before the rest of the bootstrap is configured.
pub fn embedded_args(argv: &[String]) -> Option<Vec<String>> {
    let arg0 = argv
        .first()
        .and_then(|arg| arg.rsplit('/').next())
        .unwrap_or_default();
    if matches!(arg0, "bootstrap" | "nodebootstrap") {
        return Some(argv.get(1..).unwrap_or_default().to_vec());
    }
    if matches!(argv.get(1).map(String::as_str), Some("bootstrap" | "nodebootstrap")) {
        return Some(argv.get(2..).unwrap_or_default().to_vec());
    }
    None
}

/// Runs the embedded bootstrap before Tokio is initialized by the combined
/// binary. Returns `None` when this is an ordinary component invocation.
pub fn run_embedded_from_argv(argv: &[String]) -> Option<Result<()>> {
    embedded_args(argv).map(|args| run_embedded(&args))
}

fn run_cli(args: &[String], embedded: bool) -> Result<()> {
    install_tls_provider()?;
    let command = parse_args(args)?;
    if command.help {
        print_help();
        return Ok(());
    }
    apply_root_reexec(args, embedded)?;
    dispatch(command.subcommand.as_deref())
}

fn install_tls_provider() -> Result<()> {
    if rustls::crypto::CryptoProvider::get_default().is_none() {
        rustls::crypto::ring::default_provider()
            .install_default()
            .map_err(|_| anyhow::anyhow!("could not install the rustls ring CryptoProvider"))?;
    }
    Ok(())
}

#[derive(Default)]
struct ParsedArgs {
    subcommand: Option<String>,
    help: bool,
}

fn parse_args(args: &[String]) -> Result<ParsedArgs> {
    let mut parsed = ParsedArgs::default();
    for arg in args {
        if arg == "--help" || arg == "-h" {
            parsed.help = true;
            continue;
        }
        if arg == "--with-cri" {
            std::env::set_var("NODEBOOTSTRAP_WITH_CRI", "1");
            std::env::set_var("NODELET_RUNTIME", "cri");
            continue;
        }
        if arg == "--without-cri" {
            std::env::set_var("NODEBOOTSTRAP_WITH_CRI", "0");
            std::env::set_var("NODELET_RUNTIME", "mock");
            continue;
        }
        if arg == "--from-source" || arg == "--force-source-build" {
            std::env::set_var("NODEBOOTSTRAP_SOURCE", "compile");
            continue;
        }
        if arg == "--release" || arg == "--update" {
            std::env::set_var("NODEBOOTSTRAP_SOURCE", "release");
            continue;
        }
        if let Some(value) = arg.strip_prefix("--source=") {
            anyhow::ensure!(matches!(value, "compile" | "release"), "--source must be compile or release");
            std::env::set_var("NODEBOOTSTRAP_SOURCE", value);
            continue;
        }
        if let Some(value) = arg.strip_prefix("--tag=") {
            anyhow::ensure!(!value.is_empty(), "--tag requires a release tag");
            std::env::set_var("NODEBOOTSTRAP_RELEASE_TAG", value);
            std::env::set_var("NODEBOOTSTRAP_SOURCE", "release");
            continue;
        }
        if let Some(value) = arg.strip_prefix("--layout=") {
            anyhow::ensure!(matches!(value, "split" | "combined" | "both"), "--layout must be split, combined, or both");
            std::env::set_var("NOTK8S_BUILD_LAYOUT", value);
            continue;
        }
        if let Some(value) = arg.strip_prefix("--cni=") {
            anyhow::ensure!(matches!(value, "flannel" | "none"), "--cni must be flannel or none");
            std::env::set_var("NODEBOOTSTRAP_CNI", value);
            continue;
        }
        if let Some(value) = arg.strip_prefix("--ip-family=") {
            anyhow::ensure!(matches!(value, "auto" | "ipv4" | "ipv6" | "dual"), "--ip-family must be auto, ipv4, ipv6, or dual");
            std::env::set_var("NODEBOOTSTRAP_IP_FAMILY", value);
            continue;
        }
        if let Some(value) = arg.strip_prefix("--lb-method=") {
            anyhow::ensure!(matches!(value, "random" | "round-robin" | "source-hash"), "--lb-method must be random, round-robin, or source-hash");
            std::env::set_var("NODEBOOTSTRAP_LB_METHOD", value);
            continue;
        }
        if let Some(value) = arg.strip_prefix("--proxy=") {
            anyhow::ensure!(matches!(value, "nodeproxy" | "none"), "--proxy must be nodeproxy or none");
            std::env::set_var("NODEBOOTSTRAP_PROXY", value);
            continue;
        }
        if arg == "--skip-control-plane" {
            std::env::set_var("NODEBOOTSTRAP_SKIP_CONTROL_PLANE", "1");
            continue;
        }
        if arg == "--skip-nodelet" {
            std::env::set_var("NODEBOOTSTRAP_SKIP_NODELET", "1");
            continue;
        }
        if arg == "--skip-toolchain" {
            std::env::set_var("NODEBOOTSTRAP_SKIP_TOOLCHAIN", "1");
            continue;
        }
        if arg == "--skip-containerd" {
            std::env::set_var("NODEBOOTSTRAP_SKIP_CONTAINERD", "1");
            continue;
        }
        if let Some(name) = arg.strip_prefix("--skip-") {
            let env_name = format!("NODEBOOTSTRAP_SKIP_{}", name.replace('-', "_").to_ascii_uppercase());
            std::env::set_var(env_name, "1");
            continue;
        }
        if arg == "--keep-build-tools" {
            std::env::set_var("NODEBOOTSTRAP_KEEP_BUILD_TOOLS", "1");
            continue;
        }
        if let Some(value) = arg.strip_prefix("--datastore=") {
            anyhow::ensure!(value == "nodestore", "nodebootstrap always uses nodestore; `--datastore=none` is not supported with the upstream apiserver target");
            continue;
        }
        if let Some(value) = arg.strip_prefix("--scheduler=") {
            anyhow::ensure!(value == "nodescheduler", "nodebootstrap always uses nodescheduler; the upstream target does not run a second scheduler");
            continue;
        }
        if let Some(value) = arg.strip_prefix("--controller-manager=") {
            anyhow::ensure!(value == "nodecontroller", "nodebootstrap always uses nodecontroller; the upstream target does not run a second controller manager");
            continue;
        }
        if arg.starts_with('-') {
            bail!("unknown flag '{arg}' (try --help)");
        }
        if parsed.subcommand.replace(arg.clone()).is_some() {
            bail!("only one nodebootstrap subcommand may be specified");
        }
    }
    Ok(parsed)
}

fn apply_root_reexec(args: &[String], embedded: bool) -> Result<()> {
    if is_root() {
        return Ok(());
    }
    let executable = std::env::current_exe().context("resolving nodebootstrap executable for sudo re-exec")?;
    anyhow::ensure!(crate::pkg::command_exists("sudo"), "root is required to install the control plane and services, but sudo was not found; re-run as root");
    tracing::info!("restarting nodebootstrap through sudo for the system-wide install");
    let mut command = std::process::Command::new("sudo");
    command.arg("-E").arg(executable);
    if embedded {
        command.arg("nodebootstrap");
    }
    let status = command.args(args).status().context("re-executing nodebootstrap through sudo")?;
    std::process::exit(status.code().unwrap_or(1));
}

fn is_root() -> bool {
    std::process::Command::new("id")
        .arg("-u")
        .output()
        .map(|output| String::from_utf8_lossy(&output.stdout).trim() == "0")
        .unwrap_or(false)
}

fn dispatch(subcommand: Option<&str>) -> Result<()> {
    let cfg = config::Config::from_env()?;
    match subcommand {
        Some("toolchain") => toolchain::run_with(&cfg),
        Some("containerd") => containerd::run_with(&cfg),
        Some("cni") => cni::run_with(&cfg),
        Some("fetch") => fetch::run_with(&cfg),
        Some("pki") => pki::run_with(&cfg),
        Some("kubeconfig") => kubeconfig::run_with(&cfg),
        Some("targets") => targets::run_with(&cfg),
        Some("rbac") => rbac::run_with(&cfg),
        Some("service-reconciler") => service_reconciler::run_with(&cfg),
        Some("manifests") => manifests::run_with(&cfg),
        Some("services") => services::run_with(&cfg),
        Some("nodestore") => services::ensure_nodestore(&cfg),
        Some("nodelet") => services::ensure_nodelet(&cfg),
        Some("nodeproxy") => services::ensure_nodeproxy(&cfg),
        Some("nodescheduler") => services::ensure_nodescheduler(&cfg),
        Some("nodecontroller") => services::ensure_nodecontroller(&cfg),
        Some("all") | None => run_all(),
        Some(other) => bail!("unknown subcommand '{other}' (try --help)"),
    }
}

fn print_help() {
    println!("nodebootstrap — install or update the complete not-k8s stack");
    println!();
    println!("Usage: bootstrap [options] [subcommand]");
    println!();
    println!("The default command installs/updates nodestore, upstream kube-apiserver,");
    println!("nodescheduler, nodecontroller, nodelet, nodeproxy, containerd, CNI, PKI,");
    println!("RBAC, kubeconfigs, and CoreDNS. Existing services are restarted.");
    println!();
    println!("Options:");
    println!("  --with-cri             use the real containerd/CRI runtime (default)");
    println!("  --without-cri          skip containerd/CNI and use nodelet's mock runtime");
    println!("  --from-source          build components from this checkout");
    println!("  --release [--tag=TAG]  fetch published component binaries");
    println!("  --layout=combined|split|both");
    println!("  --proxy=none           omit nodeproxy service");
    println!("  --cni=none             use an externally-managed CNI");
    println!("  --skip-control-plane   only stage services against an existing cluster");
    println!("  --skip-nodelet         do not install nodelet");
    println!("  -h, --help             show this help");
    println!();
    println!("Subcommands: all, toolchain, containerd, cni, fetch, pki, kubeconfig,");
    println!("targets, rbac, service-reconciler, manifests, services, nodestore,");
    println!("nodelet, nodeproxy, nodescheduler, nodecontroller");
}
