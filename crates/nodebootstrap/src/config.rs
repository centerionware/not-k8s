//! Env-var configuration, matching the other components' style (`nodelet`'s
//! `config.rs`, `nodeproxy`'s, etc.): everything comes from `NOTK8S_*` /
//! `NODEBOOTSTRAP_*` env vars with sane defaults, no CLI flag parser. The
//! shell entry points this crate replaces already set these same
//! environment variables today (`bootstrap-source.sh`'s `--skip-*` flags,
//! `PROXY=`, `DATASTORE=`, `SCHEDULER=`, layout selection, ...) — mapping
//! onto the same names here is what makes the cutover a drop-in rather than
//! a second config surface operators have to learn.

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
    pub skip_toolchain: bool,
    pub skip_containerd: bool,
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
    pub fn from_env() -> Result<Self> {
        let flag = |name: &str| std::env::var(name).is_ok_and(|v| v == "1" || v == "true");
        let cni_provider = match std::env::var("NODEBOOTSTRAP_CNI").as_deref() {
            Ok("none") => None,
            Ok(other) => Some(other.to_string()),
            Err(_) => Some("flannel".to_string()),
        };
        let source = match std::env::var("NODEBOOTSTRAP_SOURCE").as_deref() {
            Ok("release") => Source::Release,
            _ => Source::Compile,
        };
        let layout = match std::env::var("NOTK8S_BUILD_LAYOUT").as_deref() {
            Ok("split") => Layout::Split,
            _ => Layout::Combined,
        };
        Ok(Config {
            skip_toolchain: flag("NODEBOOTSTRAP_SKIP_TOOLCHAIN"),
            skip_containerd: flag("NODEBOOTSTRAP_SKIP_CONTAINERD"),
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
        })
    }
}
