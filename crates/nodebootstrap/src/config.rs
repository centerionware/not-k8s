//! Env-var configuration, matching the other components' style (`nodelet`'s
//! `config.rs`, `nodeproxy`'s, etc.). The public binary also translates its
//! installer flags into these variables, so a deployment can be driven either
//! by `./bootstrap` (CRI is the default) or by an already-supervised subcommand.

use anyhow::Result;

/// Where to fetch a component from, mirroring `bootstrap-release.sh`'s
/// existing choice between compiling and downloading a published artifact.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// `cargo build`, same as `bootstrap-source.sh`.
    Compile,
    /// Fetch a GitHub Release asset — `Tag(None)` means "latest".
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
/// (real `kube-apiserver` et al., no k3s); the `nodeapiserver` integration
/// branch adds `NodeApiserver` and flips the default only once that
/// component's own acceptance criteria are met.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    Upstream,
    // NodeApiserver is added on the nodeapiserver branch, in targets.rs,
    // once crates/nodeapiserver exists to point at.
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
    /// `None` skips CNI setup entirely (bring-your-own — Cilium etc.).
    /// `Some("flannel")` is the only provider this crate installs itself.
    pub cni_provider: Option<String>,
    pub source: Source,
    pub layout: Layout,
    /// Release tag to fetch when `source == Release`; `None` means latest.
    pub release_tag: Option<String>,
    pub target: Target,
    pub skip_pki: bool,
    pub skip_kubeconfig: bool,
    pub skip_rbac: bool,
    pub skip_service_reconciler: bool,
    pub skip_manifests: bool,
    /// Mirrors `bootstrap-source.sh`'s `--proxy=none` -- skip installing
    /// `nodeproxy` entirely (bring-your-own Service/ClusterIP routing, or
    /// isolating a nodelet/apiserver/datastore bug from nftables churn).
    /// `NODEBOOTSTRAP_PROXY=none` to disable; any other value (default
    /// `nodeproxy`) installs it as normal.
    pub skip_nodeproxy: bool,
    pub skip_nodelet: bool,
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
        std::env::var("NODEBOOTSTRAP_CLUSTER_DNS_IP").unwrap_or_else(|_| "10.43.0.10".to_string())
    }

    pub fn cluster_domain(&self) -> String {
        std::env::var("NODEBOOTSTRAP_CLUSTER_DOMAIN").unwrap_or_else(|_| "cluster.local".to_string())
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
        let cni_provider = match std::env::var("NODEBOOTSTRAP_CNI").as_deref() {
            Ok("none") => None,
            Ok(other) => Some(other.to_string()),
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
        Ok(Config {
            with_cri: !matches!(std::env::var("NODEBOOTSTRAP_WITH_CRI").as_deref(), Ok("0" | "false")),
            skip_toolchain: flag("NODEBOOTSTRAP_SKIP_TOOLCHAIN"),
            skip_containerd: flag("NODEBOOTSTRAP_SKIP_CONTAINERD"),
            skip_control_plane: flag("NODEBOOTSTRAP_SKIP_CONTROL_PLANE"),
            cni_provider,
            source,
            layout,
            release_tag: std::env::var("NODEBOOTSTRAP_RELEASE_TAG").ok(),
            target: Target::Upstream,
            skip_pki: flag("NODEBOOTSTRAP_SKIP_PKI"),
            skip_kubeconfig: flag("NODEBOOTSTRAP_SKIP_KUBECONFIG"),
            skip_rbac: flag("NODEBOOTSTRAP_SKIP_RBAC"),
            skip_service_reconciler: flag("NODEBOOTSTRAP_SKIP_SERVICE_RECONCILER"),
            skip_manifests: flag("NODEBOOTSTRAP_SKIP_MANIFESTS"),
            skip_nodeproxy: std::env::var("NODEBOOTSTRAP_PROXY").as_deref() == Ok("none"),
            skip_nodelet: flag("NODEBOOTSTRAP_SKIP_NODELET"),
        })
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
