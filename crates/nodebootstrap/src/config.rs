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
