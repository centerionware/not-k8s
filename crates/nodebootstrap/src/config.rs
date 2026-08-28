//! Env-var configuration, matching the other components' style (`nodelet`'s
//! `config.rs`, `nodeproxy`'s, etc.). The public binary also translates its
//! installer flags into these variables, so a deployment can be driven either
//! by `./bootstrap` (CRI is the default) or by an already-supervised subcommand.

use anyhow::{Context, Result};

const WITHOUT_FLANNEL_MARKER: &str = "without-flannel";
const ADVERTISE_ADDRESS_MARKER: &str = "advertise-address";
pub(crate) const DEFAULT_SERVICE_CIDR: &str = "10.43.0.0/16";

/// Where to fetch a component from, mirroring `bootstrap-release.sh`'s
/// existing choice between compiling and downloading a published artifact.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// `cargo build`, same as `bootstrap-source.sh`.
    Compile,
    /// Fetch a GitHub Release asset — `Tag(None)` means "latest".
    Release,
}

/// Cargo profile used by the source builder. Debug is intentionally the
/// ordinary fast profile for e2e iteration; release carries the static size
/// and optimization settings from the workspace's release profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuildProfile {
    Debug,
    Release,
}

/// Combined (`bin/notk8s`, one multi-call binary) vs. split (`bin/nodelet` +
/// `bin/nodeproxy` + ...) — see `CLAUDE.md`'s "Two build layouts" section.
/// Combined is the default here as it is release-wide.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layout {
    Combined,
    Split,
    Both,
}

/// Which apiserver/controller-manager/scheduler combination `targets/`
/// installs and points the generated PKI/kubeconfig/RBAC at. See
/// `docs/NODEBOOTSTRAP_PLAN.md`'s point 3: `main` defaults to `Upstream`
/// (real `kube-apiserver`, no k3s); the nodeapiserver integration branch
/// exposes `NodeApiserver` as an explicit target until its full acceptance
/// gate is complete.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    Upstream,
    NodeApiserver,
}

#[derive(Debug, Clone)]
pub struct Config {
    /// Real containerd/CRI is the supported deployment default. `false` is
    /// retained for fast mock-runtime development and explicitly means that
    /// containerd/CNI setup is skipped.
    pub with_cri: bool,
    pub skip_toolchain: bool,
    pub skip_containerd: bool,
    pub skip_control_plane: bool,
    /// Join an already-running Kubernetes control plane instead of installing
    /// this project's control-plane services on the host.
    pub worker: bool,
    /// Add this host as a new control-plane member of an existing nodestore
    /// cluster. This is deliberately explicit: a control-plane host is not a
    /// worker unless the operator requests the single-node default.
    pub control_plane: bool,
    /// Remove this host's control-plane member and services, retaining local
    /// data for recovery rather than deleting it implicitly.
    pub remove_control_plane: bool,
    /// Existing nodestore client endpoint used for control-plane membership
    /// changes. The worker path does not use this.
    pub control_plane_join_endpoint: Option<String>,
    /// New member's advertised nodestore peer URL.
    pub control_plane_peer_url: Option<String>,
    /// Member id to remove. Requiring it for removal avoids removing the
    /// wrong member when a host has been reconfigured.
    pub control_plane_member_id: Option<u64>,
    /// Existing kubeconfig used by worker mode. It may be a full node/admin
    /// kubeconfig, or the low-privilege bootstrap kubeconfig when
    /// `worker_bootstrap_kubeconfig` is also set.
    pub worker_kubeconfig: Option<std::path::PathBuf>,
    /// Optional kubeconfig used for nodelet's standard CSR/TLS bootstrap.
    pub worker_bootstrap_kubeconfig: Option<std::path::PathBuf>,
    /// Do not install flannel, and remember that choice for later updates.
    pub without_flannel: bool,
    /// `None` skips CNI setup entirely (bring-your-own — Cilium etc.).
    /// `Some("flannel")` is the only provider this crate installs itself.
    pub cni_provider: Option<String>,
    pub source: Source,
    pub build_profile: BuildProfile,
    pub layout: Layout,
    /// Release tag to fetch when `source == Release`; `None` means latest.
    pub release_tag: Option<String>,
    pub target: Target,
    pub skip_pki: bool,
    pub skip_kubeconfig: bool,
    pub skip_rbac: bool,
    pub skip_service_reconciler: bool,
    pub skip_manifests: bool,
    /// Do not install or configure the in-cluster CoreDNS service.
    pub disable_dns: bool,
    /// Remove nodebootstrap-managed services, files, and tracked packages.
    pub uninstall: bool,
    /// Mirrors `bootstrap-source.sh`'s `--proxy=none` -- skip installing
    /// `nodeproxy` entirely (bring-your-own Service/ClusterIP routing, or
    /// isolating a nodelet/apiserver/datastore bug from nftables churn).
    /// `NODEBOOTSTRAP_PROXY=none` to disable; any other value (default
    /// `nodeproxy`) installs it as normal.
    pub skip_nodeproxy: bool,
    pub skip_nodelet: bool,
    /// IP address passed to kube-apiserver's `--advertise-address`. When it
    /// is set, it is persisted so a later update keeps advertising the same
    /// address instead of rediscovering a different interface.
    pub advertise_address: Option<String>,
}

impl Config {
    /// Where `pki.rs` writes cert/key PEMs and `kubeconfig.rs` reads them
    /// back from -- one shared path so the two modules can't drift, since
    /// each subcommand (`nodebootstrap pki` / `nodebootstrap kubeconfig`)
    /// is independently invokable and re-reads disk state rather than
    /// passing an in-memory value between steps.
    pub fn pki_dir(&self) -> std::path::PathBuf {
        std::env::var("NODEBOOTSTRAP_PKI_DIR")
            .unwrap_or_else(|_| "/var/lib/nodebootstrap/pki".to_string())
            .into()
    }

    pub fn kubeconfig_dir(&self) -> std::path::PathBuf {
        std::env::var("NODEBOOTSTRAP_KUBECONFIG_DIR")
            .unwrap_or_else(|_| "/etc/nodebootstrap".to_string())
            .into()
    }

    /// Installation flags are kept beside the generated kubeconfigs so an
    /// update can reproduce the original topology and feature choices.
    pub fn flags_path(&self) -> std::path::PathBuf {
        std::env::var("NODEBOOTSTRAP_FLAGS_FILE")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|_| self.kubeconfig_dir().join("flags"))
    }

    pub fn state_dir(&self) -> std::path::PathBuf {
        std::env::var("NODEBOOTSTRAP_STATE_DIR")
            .unwrap_or_else(|_| "/var/lib/nodebootstrap".to_string())
            .into()
    }

    pub fn without_flannel_marker(&self) -> std::path::PathBuf {
        self.state_dir().join(WITHOUT_FLANNEL_MARKER)
    }

    pub fn advertise_address_marker(&self) -> std::path::PathBuf {
        self.state_dir().join(ADVERTISE_ADDRESS_MARKER)
    }

    /// The kubeconfig every node-side service should use to reach the API.
    /// Control-plane mode owns the generated admin config; worker mode must
    /// use the operator-supplied config and never mint a replacement CA.
    pub fn cluster_kubeconfig(&self) -> Result<std::path::PathBuf> {
        if self.worker {
            return self
                .worker_kubeconfig
                .clone()
                .context("--worker requires --kubeconfig=PATH (or KUBECONFIG) for the existing control plane");
        }
        Ok(self.kubeconfig_dir().join("admin.kubeconfig"))
    }

    /// Name of the local API-server unit that node-side services must wait
    /// for. The target is selected once at installation time, but all unit
    /// dependencies are derived from it so switching targets cannot leave
    /// nodelet, nodeproxy, CNI, or controllers ordered after a service that
    /// is not installed.
    pub fn apiserver_service(&self) -> &'static str {
        match self.target {
            Target::Upstream => "kube-apiserver.service",
            Target::NodeApiserver => "nodeapiserver.service",
        }
    }

    pub fn control_plane_join_endpoint(&self) -> Result<String> {
        self.control_plane_join_endpoint.clone().context(
            "control-plane mode requires --join=URL (or NODEBOOTSTRAP_JOIN_ENDPOINT) for an existing nodestore member",
        )
    }

    pub fn control_plane_peer_url(&self) -> Result<String> {
        self.control_plane_peer_url.clone().context(
            "control-plane mode requires --peer-url=https://HOST:2380 (or NODEBOOTSTRAP_PEER_URL)",
        )
    }

    pub fn worker_nodelet_kubeconfig(&self) -> std::path::PathBuf {
        std::env::var("NODELET_KUBECONFIG")
            .ok()
            .filter(|path| !path.is_empty())
            .map(Into::into)
            .unwrap_or_else(|| self.kubeconfig_dir().join("nodelet.kubeconfig"))
    }

    pub fn node_name(&self) -> String {
        std::env::var("NODELET_NODE_NAME").unwrap_or_else(|_| {
            std::process::Command::new("uname")
                .arg("-n")
                .output()
                .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
                .unwrap_or_default()
        })
    }

    /// Persist only the explicit operator choice. A plain `--cni=none` is
    /// still a one-run override; `--without-flannel` is the update-safe mode.
    pub fn persist_preferences(&self) -> Result<()> {
        let marker = self.without_flannel_marker();
        if self.without_flannel {
            if let Some(parent) = marker.parent() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("creating nodebootstrap state dir {}", parent.display()))?;
            }
            std::fs::write(&marker, b"external-cni\n")
                .with_context(|| format!("saving flannel-disabled preference at {}", marker.display()))?;
            tracing::info!(path = %marker.display(), "saved --without-flannel for future updates");
        } else if std::env::var("NODEBOOTSTRAP_CNI").as_deref() == Ok("flannel") {
            let _ = std::fs::remove_file(&marker);
        }
        if let Some(address) = &self.advertise_address {
            let marker = self.advertise_address_marker();
            if let Some(parent) = marker.parent() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("creating nodebootstrap state dir {}", parent.display()))?;
            }
            std::fs::write(&marker, format!("{address}\n"))
                .with_context(|| format!("saving apiserver advertise address at {}", marker.display()))?;
            tracing::info!(address, path = %marker.display(), "saved apiserver advertise address for future updates");
        }
        Ok(())
    }

    /// Nodelet's kubelet-style HTTPS server writes its self-signed serving
    /// certificate here. The apiserver needs this path as its
    /// `--kubelet-certificate-authority` once nodelet has started.
    pub fn nodelet_server_cert_dir(&self) -> std::path::PathBuf {
        std::env::var("NODELET_SERVER_CERT_DIR")
            .unwrap_or_else(|_| "/var/lib/nodelet/pki".to_string())
            .into()
    }

    pub fn nodelet_server_ca_path(&self) -> std::path::PathBuf {
        self.nodelet_server_cert_dir().join("server-ca.pem")
    }

    /// The apiserver address kubeconfigs point at. Defaults to real
    /// upstream `kube-apiserver`'s own default bind port (6443) on
    /// localhost -- correct once `targets::upstream` runs it there;
    /// overridable for anything else (a remote apiserver, a throwaway-rig
    /// test instance on a scratch port).
    pub fn apiserver_server(&self) -> String {
        std::env::var("NODEBOOTSTRAP_APISERVER_SERVER")
            .unwrap_or_else(|_| "https://127.0.0.1:6443".to_string())
    }

    /// CoreDNS's own ClusterIP. Default matches `deploy/lib/
    /// upstream-kube-apiserver.sh`'s `SERVICE_CIDR=10.43.0.0/16` default --
    /// `.10` is the conventional k8s/k3s "tenth address in the service
    /// CIDR" slot for cluster DNS.
    pub fn cluster_dns_ip(&self) -> String {
        std::env::var("NODEBOOTSTRAP_CLUSTER_DNS_IP")
            .unwrap_or_else(|_| service_cidr_address(&self.service_cidr(), 10).to_string())
    }

    /// The IPv6 CoreDNS ServiceIP, when an IPv6 service CIDR was explicitly
    /// configured with `--cidr6`. IPv4 remains the primary ClusterIP so an
    /// IPv4-only installation is unchanged.
    pub fn cluster_dns_ip6(&self) -> Option<String> {
        self.service_cidr6()
            .map(|cidr| service_cidr_address6(&cidr, 10).to_string())
    }

    pub fn cluster_dns_ips(&self) -> Vec<String> {
        let mut ips = vec![self.cluster_dns_ip()];
        if let Some(ip) = self.cluster_dns_ip6() {
            ips.push(ip);
        }
        ips
    }

    pub fn cluster_domain(&self) -> String {
        std::env::var("NODEBOOTSTRAP_CLUSTER_DOMAIN").unwrap_or_else(|_| "cluster.local".to_string())
    }

    /// The service CIDR passed to kube-apiserver. `--cidr` changes this from
    /// the historical `10.43.0.0/16` default.
    pub fn service_cidr(&self) -> String {
        std::env::var("NODEBOOTSTRAP_SERVICE_CIDR").unwrap_or_else(|_| DEFAULT_SERVICE_CIDR.to_string())
    }

    /// Optional IPv6 service network. It is intentionally opt-in: the
    /// historical default is IPv4-only, while setting `--cidr6` asks the
    /// apiserver for a dual-stack service range alongside `--cidr`.
    pub fn service_cidr6(&self) -> Option<String> {
        std::env::var("NODEBOOTSTRAP_SERVICE_CIDR6")
            .ok()
            .filter(|value| !value.is_empty())
    }

    /// The first service address in the configured service CIDR. This is the
    /// apiserver ServiceIP and is included in its serving certificate.
    pub fn service_ip(&self) -> Result<std::net::IpAddr> {
        Ok(service_cidr_address(&self.service_cidr(), 1).into())
    }

    pub fn service_ip6(&self) -> Result<Option<std::net::IpAddr>> {
        Ok(self
            .service_cidr6()
            .map(|cidr| service_cidr_address6(&cidr, 1).into()))
    }

    pub fn service_ips(&self) -> Result<Vec<std::net::IpAddr>> {
        let mut ips = vec![self.service_ip()?];
        if let Some(ip) = self.service_ip6()? {
            ips.push(ip);
        }
        Ok(ips)
    }

    pub fn cni_marker(&self) -> std::path::PathBuf {
        self.state_dir().join("cni-installed")
    }

    pub fn containerd_marker(&self) -> std::path::PathBuf {
        self.state_dir().join("containerd-installed")
    }

    /// `ipv4` | `ipv6` | `dual` -- passed straight through to `flanneld`
    /// (`cni.rs`) as `IP_FAMILY`. Matches `run-flanneld.sh`'s own default.
    pub fn ip_family(&self) -> String {
        std::env::var("NODEBOOTSTRAP_IP_FAMILY").unwrap_or_else(|_| "ipv4".to_string())
    }

    /// Matches `targets/upstream.rs`'s `--cluster-cidr` default -- the pod
    /// network flannel and `kube-controller-manager`'s node-CIDR allocator
    /// must agree on.
    pub fn ipv4_cluster_cidr(&self) -> String {
        std::env::var("NODEBOOTSTRAP_IPV4_CLUSTER_CIDR").unwrap_or_else(|_| "10.42.0.0/16".to_string())
    }

    pub fn ipv6_cluster_cidr(&self) -> String {
        std::env::var("NODEBOOTSTRAP_IPV6_CLUSTER_CIDR").unwrap_or_else(|_| "fd00:42::/56".to_string())
    }

    /// Where a fetched-but-not-packaged toolchain (rustup, an official
    /// protoc/Go release) gets unpacked and symlinked from -- same role as
    /// `bootstrap-source.sh`'s `TOOLCHAIN_DIR`. `<dir>/bin` belongs on
    /// `PATH` ahead of the system one, same as the shell version.
    pub fn toolchain_dir(&self) -> std::path::PathBuf {
        std::env::var("NODEBOOTSTRAP_TOOLCHAIN_DIR")
            .unwrap_or_else(|_| "/var/lib/nodebootstrap/toolchain".to_string())
            .into()
    }

    /// Scratch download/extract dir -- same role as `bootstrap-source.sh`'s
    /// `SRC_DIR`.
    pub fn src_dir(&self) -> std::path::PathBuf {
        std::env::var("NODEBOOTSTRAP_SRC_DIR")
            .unwrap_or_else(|_| "/var/lib/nodebootstrap/src".to_string())
            .into()
    }

    /// Where `service_mgr.rs`'s fallback tier writes its supervisor
    /// scripts and pid files -- same role as `bootstrap-source.sh`'s
    /// `WORK_DIR`.
    pub fn work_dir(&self) -> std::path::PathBuf {
        std::env::var("NODEBOOTSTRAP_WORK_DIR").unwrap_or_else(|_| "/var/lib/nodebootstrap/work".to_string()).into()
    }

    /// Where a fallback-tier supervised service's stdout/stderr goes --
    /// same role as `bootstrap-source.sh`'s `LOG_DIR`.
    pub fn log_dir(&self) -> std::path::PathBuf {
        std::env::var("NODEBOOTSTRAP_LOG_DIR").unwrap_or_else(|_| "/var/log/nodebootstrap".to_string()).into()
    }

    /// Host architecture, normalized to the same vocabulary
    /// `deploy/lib/common.sh`'s `$ARCH` uses (`x86_64`, `aarch64`,
    /// `armv7l`, ...) -- read from `uname -m` once, not re-shelled per
    /// caller.
    pub fn arch(&self) -> String {
        std::env::var("NODEBOOTSTRAP_ARCH").unwrap_or_else(|_| {
            std::process::Command::new("uname")
                .arg("-m")
                .output()
                .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
                .unwrap_or_else(|_| "unknown".to_string())
        })
    }
    pub fn nodelet_runtime(&self) -> String {
        std::env::var("NODELET_RUNTIME").unwrap_or_else(|_| {
            if self.with_cri { "cri" } else { "mock" }.to_string()
        })
    }

    pub fn from_env() -> Result<Self> {
        let flag = |name: &str| std::env::var(name).is_ok_and(|v| v == "1" || v == "true");
        let worker = flag("NODEBOOTSTRAP_WORKER");
        let control_plane = flag("NODEBOOTSTRAP_CONTROL_PLANE");
        let remove_control_plane = flag("NODEBOOTSTRAP_REMOVE_CONTROL_PLANE");
        anyhow::ensure!(
            [worker, control_plane, remove_control_plane].into_iter().filter(|set| *set).count() <= 1,
            "--worker, --control-plane, and --remove-control-plane are mutually exclusive"
        );
        let worker_bootstrap_kubeconfig = std::env::var("NODEBOOTSTRAP_WORKER_BOOTSTRAP_KUBECONFIG")
            .ok()
            .filter(|path| !path.is_empty())
            .map(std::path::PathBuf::from);
        let worker_kubeconfig = std::env::var("NODEBOOTSTRAP_WORKER_KUBECONFIG")
            .ok()
            .filter(|path| !path.is_empty())
            .or_else(|| worker_bootstrap_kubeconfig.as_ref().map(|path| path.to_string_lossy().into_owned()))
            .or_else(|| worker.then(|| std::env::var("KUBECONFIG").ok()).flatten())
            .filter(|path| !path.is_empty())
            .map(std::path::PathBuf::from);
        let explicit_cni = std::env::var("NODEBOOTSTRAP_CNI");
        let service_cidr = std::env::var("NODEBOOTSTRAP_SERVICE_CIDR")
            .unwrap_or_else(|_| DEFAULT_SERVICE_CIDR.to_string());
        validate_service_cidr(&service_cidr)?;
        if let Some(service_cidr6) = std::env::var("NODEBOOTSTRAP_SERVICE_CIDR6")
            .ok()
            .filter(|value| !value.is_empty())
        {
            validate_service_cidr6(&service_cidr6)?;
        }
        let state_dir = std::env::var("NODEBOOTSTRAP_STATE_DIR")
            .unwrap_or_else(|_| "/var/lib/nodebootstrap".to_string());
        let advertise_address = std::env::var("NODEBOOTSTRAP_ADVERTISE_ADDRESS")
            .ok()
            .filter(|value| !value.is_empty())
            .or_else(|| {
                std::fs::read_to_string(std::path::Path::new(&state_dir).join(ADVERTISE_ADDRESS_MARKER))
                    .ok()
                    .map(|value| value.trim().to_string())
                    .filter(|value| !value.is_empty())
            });
        if let Some(address) = &advertise_address {
            anyhow::ensure!(
                address.parse::<std::net::IpAddr>().is_ok(),
                "NODEBOOTSTRAP_ADVERTISE_ADDRESS must be an IP address, got '{address}'"
            );
        }
        let marker_exists = std::path::Path::new(&state_dir)
            .join(WITHOUT_FLANNEL_MARKER)
            .is_file();
        let without_flannel = !matches!(explicit_cni.as_deref(), Ok("flannel"))
            && (flag("NODEBOOTSTRAP_WITHOUT_FLANNEL") || (explicit_cni.is_err() && marker_exists));
        let cni_provider = match explicit_cni.as_deref() {
            Ok("none") => None,
            Ok(other) => Some(other.to_string()),
            Err(_) if worker || control_plane || without_flannel => None,
            Err(_) => Some("flannel".to_string()),
        };
        let source = match std::env::var("NODEBOOTSTRAP_SOURCE").as_deref() {
            Ok("release") => Source::Release,
            Ok("compile") => Source::Compile,
            _ if workspace_checkout_present() => Source::Compile,
            _ => Source::Release,
        };
        let layout = match std::env::var("NOTK8S_BUILD_LAYOUT").as_deref() {
            Ok("split") => Layout::Split,
            Ok("both") => Layout::Both,
            _ => Layout::Combined,
        };
        let build_profile = match std::env::var("NOTK8S_BUILD_PROFILE").as_deref() {
            Ok("debug") => BuildProfile::Debug,
            Ok("release") | Err(_) => BuildProfile::Release,
            Ok(other) => anyhow::bail!("NOTK8S_BUILD_PROFILE must be debug or release, got '{other}'"),
        };
        Ok(Config {
            with_cri: !matches!(std::env::var("NODEBOOTSTRAP_WITH_CRI").as_deref(), Ok("0" | "false")),
            skip_toolchain: flag("NODEBOOTSTRAP_SKIP_TOOLCHAIN"),
            skip_containerd: flag("NODEBOOTSTRAP_SKIP_CONTAINERD"),
            skip_control_plane: flag("NODEBOOTSTRAP_SKIP_CONTROL_PLANE"),
            worker,
            control_plane,
            remove_control_plane,
            control_plane_join_endpoint: std::env::var("NODEBOOTSTRAP_JOIN_ENDPOINT")
                .ok()
                .filter(|value| !value.is_empty()),
            control_plane_peer_url: std::env::var("NODEBOOTSTRAP_PEER_URL")
                .ok()
                .filter(|value| !value.is_empty()),
            control_plane_member_id: std::env::var("NODEBOOTSTRAP_MEMBER_ID")
                .ok()
                .filter(|value| !value.is_empty())
                .map(|value| value.parse::<u64>())
                .transpose()
                .context("NODEBOOTSTRAP_MEMBER_ID must be an integer")?,
            worker_kubeconfig,
            worker_bootstrap_kubeconfig,
            without_flannel,
            cni_provider,
            source,
            build_profile,
            layout,
            release_tag: std::env::var("NODEBOOTSTRAP_RELEASE_TAG").ok(),
            target: match std::env::var("NODEBOOTSTRAP_APISERVER").as_deref() {
                Ok("nodeapiserver") => Target::NodeApiserver,
                Ok("upstream") | Err(_) => Target::Upstream,
                Ok(other) => anyhow::bail!(
                    "NODEBOOTSTRAP_APISERVER must be upstream or nodeapiserver, got '{other}'"
                ),
            },
            skip_pki: flag("NODEBOOTSTRAP_SKIP_PKI"),
            skip_kubeconfig: flag("NODEBOOTSTRAP_SKIP_KUBECONFIG"),
            skip_rbac: flag("NODEBOOTSTRAP_SKIP_RBAC"),
            skip_service_reconciler: flag("NODEBOOTSTRAP_SKIP_SERVICE_RECONCILER"),
            skip_manifests: flag("NODEBOOTSTRAP_SKIP_MANIFESTS"),
            disable_dns: flag("NODEBOOTSTRAP_DISABLE_DNS"),
            uninstall: flag("NODEBOOTSTRAP_UNINSTALL"),
            skip_nodeproxy: std::env::var("NODEBOOTSTRAP_PROXY").as_deref() == Ok("none") || control_plane,
            skip_nodelet: flag("NODEBOOTSTRAP_SKIP_NODELET") || control_plane,
            advertise_address,
        })
    }
}

pub(crate) fn validate_service_cidr(value: &str) -> Result<()> {
    let (address, prefix) = value
        .split_once('/')
        .context("service CIDR must be an IPv4 network such as 10.43.0.0/16")?;
    let address = address
        .parse::<std::net::Ipv4Addr>()
        .with_context(|| format!("service CIDR has an invalid IPv4 address: {value}"))?;
    let prefix = prefix
        .parse::<u8>()
        .with_context(|| format!("service CIDR has an invalid prefix length: {value}"))?;
    anyhow::ensure!(prefix <= 28, "service CIDR must have at least 11 usable addresses for the apiserver and CoreDNS: {value}");
    let address = u32::from(address);
    let mask = if prefix == 0 { 0 } else { u32::MAX << (32 - prefix) };
    anyhow::ensure!(address & mask == address, "service CIDR must use a network address: {value}");
    Ok(())
}

pub(crate) fn validate_service_cidr6(value: &str) -> Result<()> {
    let (address, prefix) = value
        .split_once('/')
        .context("IPv6 service CIDR must be a network such as fd00:43::/112")?;
    let address = address
        .parse::<std::net::Ipv6Addr>()
        .with_context(|| format!("IPv6 service CIDR has an invalid address: {value}"))?;
    let prefix = prefix
        .parse::<u8>()
        .with_context(|| format!("IPv6 service CIDR has an invalid prefix length: {value}"))?;
    anyhow::ensure!(prefix <= 124, "IPv6 service CIDR must have at least 11 addresses for the apiserver and CoreDNS: {value}");
    let address = u128::from(address);
    let mask = if prefix == 0 { 0 } else { u128::MAX << (128 - prefix) };
    anyhow::ensure!(address & mask == address, "IPv6 service CIDR must use a network address: {value}");
    Ok(())
}

fn service_cidr_address(value: &str, offset: u32) -> std::net::Ipv4Addr {
    let (address, prefix) = value.split_once('/').expect("validated service CIDR");
    let address: std::net::Ipv4Addr = address.parse().expect("validated service CIDR address");
    let prefix: u8 = prefix.parse().expect("validated service CIDR prefix");
    let base = u32::from(address);
    let mask = if prefix == 0 { 0 } else { u32::MAX << (32 - prefix) };
    std::net::Ipv4Addr::from(base | (offset & !mask))
}

fn service_cidr_address6(value: &str, offset: u32) -> std::net::Ipv6Addr {
    let (address, prefix) = value.split_once('/').expect("validated IPv6 service CIDR");
    let address: std::net::Ipv6Addr = address.parse().expect("validated IPv6 service CIDR address");
    let prefix: u8 = prefix.parse().expect("validated IPv6 service CIDR prefix");
    let base = u128::from(address);
    let mask = if prefix == 0 { 0 } else { u128::MAX << (128 - prefix) };
    std::net::Ipv6Addr::from(base | (u128::from(offset) & !mask))
}

#[cfg(test)]
mod tests {
    use super::{service_cidr_address, service_cidr_address6, validate_service_cidr, validate_service_cidr6};

    #[test]
    fn validates_service_cidrs_and_derives_reserved_addresses() {
        validate_service_cidr("10.99.0.0/16").expect("valid service CIDR");
        assert_eq!(service_cidr_address("10.99.0.0/16", 1).to_string(), "10.99.0.1");
        assert_eq!(service_cidr_address("10.99.0.0/16", 10).to_string(), "10.99.0.10");
        validate_service_cidr6("fd00:99::/112").expect("valid IPv6 service CIDR");
        assert_eq!(service_cidr_address6("fd00:99::/112", 1).to_string(), "fd00:99::1");
        assert_eq!(service_cidr_address6("fd00:99::/112", 10).to_string(), "fd00:99::a");
    }

    #[test]
    fn rejects_cidrs_that_cannot_hold_the_reserved_addresses() {
        assert!(validate_service_cidr("10.99.0.1/16").is_err());
        assert!(validate_service_cidr("10.99.0.0/29").is_err());
        assert!(validate_service_cidr("fd00::/64").is_err());
        assert!(validate_service_cidr6("fd00:99::1/112").is_err());
        assert!(validate_service_cidr6("fd00:99::/125").is_err());
    }
}

fn workspace_checkout_present() -> bool {
    if let Ok(root) = std::env::var("NODEBOOTSTRAP_REPO_ROOT") {
        return std::path::Path::new(&root).join("Cargo.toml").is_file();
    }

    let Ok(mut dir) = std::env::current_dir() else { return false };
    loop {
        let candidate = dir.join("Cargo.toml");
        if std::fs::read_to_string(&candidate).is_ok_and(|contents| contents.contains("[workspace]")) {
            return true;
        }
        if !dir.pop() {
            return false;
        }
    }
}
