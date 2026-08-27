//! Library surface for `nodebootstrap`. See `docs/NODEBOOTSTRAP_PLAN.md` for
//! the module-to-shell-script mapping, the Phase 1 / Phase 2 split, and the
//! "Implementation status" table tracking which modules have real logic
//! versus a documented scope cut (most do now; each module's own doc
//! comment states its cut precisely). Land real logic group by group, each
//! its own branch/PR/e2e case per `CLAUDE.md`'s merge protocol.

pub mod config;
pub mod containerd;
pub mod cni;
pub mod cluster;
pub mod components;
pub mod e2e;
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
/// -> nodecontroller long enough for node-CIDR allocation -> nodescheduler
/// -> CNI seed Pod and readiness -> apiserver network endpoint refresh ->
/// nodecontroller and nodescheduler restart -> the remaining replacement
/// services.
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
/// apiserver to trust that CA. The replacement controller is deliberately
/// started after the RBAC authorizer barrier, before CNI readiness, because
/// its node-ipam controller allocates the PodCIDR that flanneld needs before
/// it can lease this node a subnet. Once that subnet exists, the final
/// apiserver network-endpoint refresh is performed and nodecontroller is
/// restarted so neither it nor the scheduler keeps watches from the
/// pre-refresh apiserver instance. The scheduler is started before the CNI
/// barrier so a disposable seed Pod can be scheduled and nodelet can create
/// cni0 for the endpoint refresh to discover, then restarted after that
/// refresh.
pub fn run_all() -> Result<()> {
    let cfg = config::Config::from_env()?;
    if cfg.remove_control_plane {
        cluster::remove_existing(&cfg)?;
        services::remove_control_plane(&cfg);
        return Ok(());
    }
    cfg.persist_preferences()?;

    // A worker never creates a CA, kubeconfig, datastore, apiserver, or
    // controller. It consumes the operator-supplied kubeconfig and installs
    // only the node-side services. The optional flannel path is explicit;
    // the normal worker path leaves CNI entirely to the existing cluster.
    if cfg.worker {
        if matches!(cfg.source, config::Source::Compile) && !fetch::has_prebuilt() {
            toolchain::run_with(&cfg)?;
        }
        if cfg.with_cri && !cfg.skip_containerd {
            containerd::run_with(&cfg)?;
        }
        fetch::run_with(&cfg)?;
        if cfg.with_cri || cfg.cni_provider.is_none() {
            cni::run_with(&cfg)?;
        }
        services::ensure_nodelet(&cfg)?;
        services::ensure_nodeproxy(&cfg)?;
        return Ok(());
    }

    if matches!(cfg.source, config::Source::Compile) && !fetch::has_prebuilt() {
        toolchain::run_with(&cfg)?;
    }
    if cfg.with_cri && !cfg.skip_containerd {
        containerd::run_with(&cfg)?;
    }
    fetch::run_with(&cfg)?;
    if cfg.control_plane {
        // A joining apiserver must share the existing cluster CA and
        // ServiceAccount signing key. It may not create a parallel cluster.
        pki::require_existing(&cfg)?;
        cluster::join_existing(&cfg)?;
    } else {
        pki::run_with(&cfg)?;
    }
    kubeconfig::run_with(&cfg)?;

    if !cfg.skip_control_plane {
        services::ensure_nodestore(&cfg)?;
        targets::run_with(&cfg)?;
        if cfg.with_cri || cfg.cni_provider.is_none() {
            cni::run_with(&cfg)?;
        }
        if !cfg.control_plane {
            service_reconciler::run_with(&cfg)?;
            manifests::run_with(&cfg)?;
        }
    }

    if !cfg.skip_nodelet {
        services::ensure_nodelet(&cfg)?;
        if !cfg.skip_control_plane && cfg.with_cri {
            targets::enable_nodelet_proxy(&cfg)?;
        }
    } else if cfg.control_plane {
        services::remove_nodelet(&cfg);
    }
    if !cfg.skip_control_plane {
        // RBAC must be present before nodecontroller starts. With flannel,
        // nodecontroller then needs to run briefly so node-ipam allocates the
        // PodCIDR that flanneld needs before it can write subnet.env.
        rbac::run_with(&cfg)?;
        if !cfg.skip_nodelet
            && cfg.with_cri
            && cfg.cni_provider.as_deref() == Some("flannel")
        {
            services::ensure_nodecontroller(&cfg)?;
            services::ensure_nodescheduler(&cfg)?;
            cni::wait_for_flannel_subnet(&cfg)?;

            // This is the final kube-apiserver restart: the first instance
            // starts before cni0 exists and advertises loopback, while pods
            // need the bridge address to reach the apiserver Service. The
            // controller that allocated the subnet is restarted below after
            // this refresh, so neither replacement controller begins its
            // normal watch lifecycle against the old apiserver instance.
            targets::refresh_network_advertise_address(&cfg)?;
        }
        services::ensure_nodecontroller(&cfg)?;
        services::ensure_nodescheduler(&cfg)?;
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
    let persisted = load_persisted_flags()?;
    let explicit_source = persisted.iter().chain(args.iter()).any(|arg| {
        matches!(arg.as_str(), "--from-source" | "--force-source-build" | "--release")
            || arg.starts_with("--source=")
            || arg.starts_with("--tag=")
            || arg.starts_with("--profile=")
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
    let persisted = load_persisted_flags()?;
    let effective = effective_args(&persisted, args);
    let command = parse_args(&effective)?;
    if command.help {
        print_help();
        return Ok(());
    }
    if command.e2e_list {
        anyhow::ensure!(command.subcommand.is_none(), "--e2e-list cannot be combined with a subcommand");
        return e2e::list(command.only.as_deref(), command.shard.as_deref());
    }
    if command.e2e_needs_drivers {
        anyhow::ensure!(
            command.subcommand.is_none(),
            "--e2e-needs-drivers cannot be combined with a subcommand"
        );
        return e2e::needs_drivers(command.only.as_deref(), command.shard.as_deref());
    }
    if command.e2e {
        anyhow::ensure!(command.subcommand.is_none(), "--e2e cannot be combined with a subcommand");
        return e2e::run(command.only.as_deref(), command.shard.as_deref());
    }
    anyhow::ensure!(command.only.is_none(), "--only is only valid with --e2e");
    anyhow::ensure!(command.shard.is_none(), "--shard is only valid with --e2e");
    apply_root_reexec(args, embedded)?;
    persist_installation_flags(&merge_installation_flags(&persisted, args))?;
    dispatch(command.subcommand.as_deref())
}

fn flags_path() -> std::path::PathBuf {
    std::env::var("NODEBOOTSTRAP_FLAGS_FILE")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| {
            std::env::var("NODEBOOTSTRAP_KUBECONFIG_DIR")
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|_| std::path::PathBuf::from("/etc/nodebootstrap"))
                .join("flags")
        })
}

fn load_persisted_flags() -> Result<Vec<String>> {
    let path = flags_path();
    match std::fs::read_to_string(&path) {
        Ok(contents) => Ok(contents
            .lines()
            .filter(|line| !line.is_empty())
            .map(str::to_owned)
            .collect()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(error).with_context(|| format!("reading persisted bootstrap flags from {}", path.display())),
    }
}

/// Keep command-line installation choices, but never replay one-shot
/// inspection/test controls or the destructive control-plane removal action.
fn is_persisted_installation_flag(arg: &str) -> bool {
    arg.starts_with("--")
        && arg != "--help"
        && arg != "--e2e"
        && arg != "--e2e-list"
        && arg != "--e2e-needs-drivers"
        && arg != "--remove-control-plane"
        && arg != "--update"
        && !arg.starts_with("--only=")
        && !arg.starts_with("--shard=")
}

fn installation_flag_key(arg: &str) -> Option<&str> {
    if !is_persisted_installation_flag(arg) {
        return None;
    }
    if matches!(arg, "--from-source" | "--force-source-build" | "--release") || arg.starts_with("--source=") {
        return Some("--source");
    }
    if arg.starts_with("--tag=") {
        return Some("--tag");
    }
    if arg == "--without-flannel" || arg.starts_with("--cni=") {
        return Some("--cni");
    }
    if arg == "--without-cri" {
        return Some("--cri");
    }
    Some(arg.split('=').next().unwrap_or(arg))
}

fn is_role_flag(arg: &str) -> bool {
    matches!(arg, "--worker" | "--control-plane" | "--remove-control-plane")
}

fn merge_installation_flags(previous: &[String], current: &[String]) -> Vec<String> {
    let mut merged: Vec<String> = Vec::new();
    for arg in previous.iter().filter(|arg| is_persisted_installation_flag(arg)) {
        if is_role_flag(arg) {
            merged.retain(|old| !is_role_flag(old));
        }
        merged.push(arg.clone());
    }
    for arg in current {
        if is_role_flag(arg) {
            merged.retain(|old| !is_role_flag(old));
            if arg != "--remove-control-plane" {
                merged.push(arg.clone());
            }
            continue;
        }
        if !is_persisted_installation_flag(arg) {
            continue;
        }
        let Some(key) = installation_flag_key(arg) else { continue };
        // A source selector without an explicit tag means "latest" or
        // "compile"; do not leave a previous --tag behind to override it.
        if key == "--source" {
            merged.retain(|old| installation_flag_key(old) != Some("--source") && installation_flag_key(old) != Some("--tag"));
        } else {
            merged.retain(|old| installation_flag_key(old) != Some(key));
        }
        merged.push(arg.clone());
    }
    merged
}

fn effective_args(previous: &[String], current: &[String]) -> Vec<String> {
    let mut effective = merge_installation_flags(previous, current);
    effective.extend(
        current
            .iter()
            .filter(|arg| !is_persisted_installation_flag(arg))
            .cloned(),
    );
    effective
}

fn persist_installation_flags(flags: &[String]) -> Result<()> {
    let path = flags_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating bootstrap flag directory {}", parent.display()))?;
    }
    let mut options = std::fs::OpenOptions::new();
    options.create(true).truncate(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(&path)
        .with_context(|| format!("opening persisted bootstrap flags {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // The file contains installation arguments, not credentials. Keep it
        // readable so an unprivileged `--e2e`/diagnostic invocation can reuse
        // the installed cluster domain and feature choices before any sudo
        // re-exec is needed.
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644))?;
    }
    use std::io::Write;
    for flag in flags {
        writeln!(file, "{flag}")?;
    }
    file.sync_all()
        .with_context(|| format!("flushing persisted bootstrap flags {}", path.display()))?;
    Ok(())
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
    e2e: bool,
    e2e_list: bool,
    e2e_needs_drivers: bool,
    only: Option<String>,
    shard: Option<String>,
    help: bool,
}

fn parse_args(args: &[String]) -> Result<ParsedArgs> {
    let mut parsed = ParsedArgs::default();
    for arg in args {
        if arg == "--help" || arg == "-h" {
            parsed.help = true;
            continue;
        }
        if arg == "--e2e" {
            parsed.e2e = true;
            continue;
        }
        if arg == "--e2e-list" {
            parsed.e2e = true;
            parsed.e2e_list = true;
            continue;
        }
        if arg == "--e2e-needs-drivers" {
            parsed.e2e = true;
            parsed.e2e_needs_drivers = true;
            continue;
        }
        if let Some(value) = arg.strip_prefix("--only=") {
            anyhow::ensure!(!value.is_empty(), "--only requires a test name or substring");
            parsed.only = Some(value.to_string());
            continue;
        }
        if let Some(value) = arg.strip_prefix("--shard=") {
            anyhow::ensure!(!value.is_empty(), "--shard requires N/5");
            parsed.shard = Some(value.to_string());
            continue;
        }
        if arg == "--worker" {
            std::env::set_var("NODEBOOTSTRAP_WORKER", "1");
            continue;
        }
        if arg == "--control-plane" {
            std::env::set_var("NODEBOOTSTRAP_CONTROL_PLANE", "1");
            continue;
        }
        if arg == "--remove-control-plane" {
            std::env::set_var("NODEBOOTSTRAP_REMOVE_CONTROL_PLANE", "1");
            continue;
        }
        if arg == "--without-flannel" {
            std::env::set_var("NODEBOOTSTRAP_WITHOUT_FLANNEL", "1");
            std::env::set_var("NODEBOOTSTRAP_CNI", "none");
            continue;
        }
        if arg == "--disable-dns" {
            std::env::set_var("NODEBOOTSTRAP_DISABLE_DNS", "1");
            continue;
        }
        if let Some(value) = arg.strip_prefix("--cluster-domain=") {
            anyhow::ensure!(!value.is_empty(), "--cluster-domain requires a DNS domain");
            std::env::set_var("NODEBOOTSTRAP_CLUSTER_DOMAIN", value);
            continue;
        }
        if let Some(value) = arg.strip_prefix("--kubeconfig=") {
            anyhow::ensure!(!value.is_empty(), "--kubeconfig requires a path");
            std::env::set_var("NODEBOOTSTRAP_WORKER_KUBECONFIG", value);
            continue;
        }
        if let Some(value) = arg.strip_prefix("--bootstrap-kubeconfig=") {
            anyhow::ensure!(!value.is_empty(), "--bootstrap-kubeconfig requires a path");
            std::env::set_var("NODEBOOTSTRAP_WORKER_BOOTSTRAP_KUBECONFIG", value);
            std::env::set_var("NODEBOOTSTRAP_WORKER_KUBECONFIG", value);
            continue;
        }
        if let Some(value) = arg.strip_prefix("--join=") {
            anyhow::ensure!(!value.is_empty(), "--join requires an existing nodestore endpoint");
            std::env::set_var("NODEBOOTSTRAP_JOIN_ENDPOINT", value);
            continue;
        }
        if let Some(value) = arg.strip_prefix("--peer-url=") {
            anyhow::ensure!(!value.is_empty(), "--peer-url requires the new node's https peer URL");
            std::env::set_var("NODEBOOTSTRAP_PEER_URL", value);
            continue;
        }
        if let Some(value) = arg.strip_prefix("--advertise-address=") {
            anyhow::ensure!(!value.is_empty(), "--advertise-address requires an IP address");
            value
                .parse::<std::net::IpAddr>()
                .context("--advertise-address must be an IP address")?;
            std::env::set_var("NODEBOOTSTRAP_ADVERTISE_ADDRESS", value);
            continue;
        }
        if let Some(value) = arg.strip_prefix("--join-ca=") {
            anyhow::ensure!(!value.is_empty(), "--join-ca requires a CA bundle path");
            std::env::set_var("NODEBOOTSTRAP_JOIN_CA_FILE", value);
            continue;
        }
        if let Some(value) = arg.strip_prefix("--join-cert=") {
            anyhow::ensure!(!value.is_empty(), "--join-cert requires a client certificate path");
            std::env::set_var("NODEBOOTSTRAP_JOIN_CERT_FILE", value);
            continue;
        }
        if let Some(value) = arg.strip_prefix("--join-key=") {
            anyhow::ensure!(!value.is_empty(), "--join-key requires a client key path");
            std::env::set_var("NODEBOOTSTRAP_JOIN_KEY_FILE", value);
            continue;
        }
        if let Some(value) = arg.strip_prefix("--member-id=") {
            anyhow::ensure!(!value.is_empty(), "--member-id requires an integer");
            value.parse::<u64>().context("--member-id must be an integer")?;
            std::env::set_var("NODEBOOTSTRAP_MEMBER_ID", value);
            continue;
        }
        if let Some(value) = arg.strip_prefix("--node-name=") {
            anyhow::ensure!(!value.is_empty(), "--node-name requires a Kubernetes node name");
            std::env::set_var("NODELET_NODE_NAME", value);
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
        if let Some(value) = arg.strip_prefix("--profile=") {
            anyhow::ensure!(matches!(value, "debug" | "release"), "--profile must be debug or release");
            std::env::set_var("NOTK8S_BUILD_PROFILE", value);
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
    // `sudo -E` is subject to the host's sudoers environment policy. In
    // particular, CI cross-builds can lose the target-specific linker and C
    // compiler variables here, causing the root re-exec to rediscover a
    // host-only musl-gcc (or fetch musl.cc again) instead of using the
    // compiler selected by the caller. Pass the build-related variables as
    // explicit `env` assignments as well as asking sudo to preserve the
    // ordinary bootstrap environment.
    command.arg("-E").arg("/usr/bin/env");
    for (key, value) in std::env::vars().filter(|(key, _)| reexec_env_key(key)) {
        command.arg(format!("{key}={value}"));
    }
    command.arg(executable);
    if embedded {
        command.arg("nodebootstrap");
    }
    let status = command.args(args).status().context("re-executing nodebootstrap through sudo")?;
    std::process::exit(status.code().unwrap_or(1));
}

fn reexec_env_key(key: &str) -> bool {
    matches!(key, "HOME" | "PATH" | "PROTOC" | "RUSTFLAGS" | "RUSTUP_HOME" | "RUSTUP_TOOLCHAIN")
        || key == "CARGO_HOME"
        || key.starts_with("CARGO_")
        || key.starts_with("CC_")
        || key.starts_with("MUSL_")
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
        Some("flanneld") => cni::run_flanneld(),
        Some("fetch") => {
            if matches!(cfg.source, config::Source::Compile) && !fetch::has_prebuilt() {
                toolchain::run_with(&cfg)?;
            }
            fetch::run_with(&cfg)
        }
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
    println!("With no role flag, the command installs/updates a single-node cluster");
    println!("including nodestore, upstream kube-apiserver, nodecontroller,");
    println!("nodescheduler, nodelet, nodeproxy, containerd, CNI, PKI, and CoreDNS.");
    println!("Existing services are restarted.");
    println!();
    println!("Options:");
    println!("  --without-cri          skip containerd/CNI and use nodelet's mock runtime");
    println!("  --from-source          build components from this checkout");
    println!("  --release [--tag=TAG]  fetch published component binaries");
    println!("  --layout=combined|split|both");
    println!("  --profile=debug|release  select the source-build Cargo profile");
    println!("  --proxy=none           omit nodeproxy service");
    println!("  --without-flannel      skip flannel and remember external CNI for updates");
    println!("  --cni=none             use an externally-managed CNI for this run");
    println!("  --disable-dns          do not install or configure CoreDNS");
    println!("  --cluster-domain=NAME  use NAME instead of cluster.local");
    println!("  --worker               install nodelet+nodeproxy against --kubeconfig only");
    println!("  --kubeconfig=PATH      existing control-plane kubeconfig for --worker");
    println!("  --bootstrap-kubeconfig=PATH  TLS bootstrap kubeconfig for --worker");
    println!("  --control-plane        join nodestore and install control-plane services only");
    println!("  --join=URL             existing nodestore endpoint for control-plane membership");
    println!("  --peer-url=URL         this node's advertised nodestore peer URL");
    println!("  --advertise-address=IP kube-apiserver address advertised to the cluster");
    println!("  --join-ca=PATH         CA for nodestore membership RPCs");
    println!("  --join-cert=PATH       client certificate for membership RPCs");
    println!("  --join-key=PATH        client key for membership RPCs");
    println!("  --remove-control-plane remove this member and its local control-plane services");
    println!("  --member-id=N          member id to remove with --remove-control-plane");
    println!("  --node-name=NAME       Kubernetes node name (defaults to hostname)");
    println!("  --e2e                  run bootstrap-native end-to-end checks");
    println!("  --e2e-list             list selected e2e checks without contacting a cluster");
    println!("  --e2e-needs-drivers    print whether selected e2e checks need CSI/DRA setup");
    println!("  --only=TEST[,TEST...]  select e2e tests by name substring");
    println!("  --shard=N/5            run one of the five CI e2e shards");
    println!("  --skip-control-plane   legacy: stage services against an existing cluster");
    println!("  --skip-nodelet         do not install nodelet");
    println!("  -h, --help             show this help");
    println!();
    println!("Subcommands: all, toolchain, containerd, cni, flanneld, fetch, pki, kubeconfig,");
    println!("targets, rbac, service-reconciler, manifests, services, nodestore,");
    println!("nodelet, nodeproxy, nodescheduler, nodecontroller");
}

#[cfg(test)]
mod tests {
    use super::{effective_args, is_persisted_installation_flag, merge_installation_flags, parse_args, reexec_env_key};

    #[test]
    fn cri_is_selected_by_default_without_a_positive_flag() {
        let parsed = parse_args(&[]).expect("default arguments should parse");
        assert!(!parsed.e2e);
        assert!(parsed.only.is_none());
    }

    #[test]
    fn with_cri_is_not_a_nodebootstrap_flag() {
        assert!(parse_args(&["--with-cri".to_string()]).is_err());
    }

    #[test]
    fn dns_flags_are_accepted() {
        assert!(parse_args(&[
            "--disable-dns".to_string(),
            "--cluster-domain=cluster.example".to_string(),
        ])
        .is_ok());
    }

    #[test]
    fn persisted_flags_exclude_one_shot_controls_and_removal() {
        assert!(is_persisted_installation_flag("--cluster-domain=cluster.example"));
        assert!(!is_persisted_installation_flag("--e2e"));
        assert!(!is_persisted_installation_flag("--only=dns"));
        assert!(!is_persisted_installation_flag("--remove-control-plane"));
        assert!(!is_persisted_installation_flag("--update"));
    }

    #[test]
    fn current_installation_flags_override_saved_choices() {
        let previous = vec!["--disable-dns".to_string(), "--cluster-domain=old.example".to_string()];
        let current = vec!["--cluster-domain=new.example".to_string()];
        assert_eq!(
            merge_installation_flags(&previous, &current),
            vec!["--disable-dns", "--cluster-domain=new.example"]
        );
    }

    #[test]
    fn current_role_replaces_saved_role_before_parse_args() {
        let effective = effective_args(&["--worker".to_string()], &["--control-plane".to_string()]);
        assert_eq!(effective, vec!["--control-plane"]);
        assert!(parse_args(&effective).is_ok());
    }

    #[test]
    fn removing_a_role_removes_it_before_parse_args() {
        let effective = effective_args(&["--control-plane".to_string()], &["--remove-control-plane".to_string()]);
        assert_eq!(effective, vec!["--remove-control-plane"]);
        assert!(parse_args(&effective).is_ok());
    }

    #[test]
    fn root_reexec_preserves_target_build_environment() {
        assert!(reexec_env_key("MUSL_C_COMPILER"));
        assert!(reexec_env_key("CC_aarch64_unknown_linux_musl"));
        assert!(reexec_env_key("CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER"));
        assert!(reexec_env_key("CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_RUSTFLAGS"));
        assert!(!reexec_env_key("GITHUB_TOKEN"));
    }
}
