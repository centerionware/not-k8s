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

use anyhow::Result;

/// Runs every phase in dependency order: toolchain -> containerd -> fetch
/// -> pki -> kubeconfig -> targets (install/start the apiserver) -> cni ->
/// rbac -> service-reconciler -> manifests. This is what
/// `bootstrap-source.sh`/`bootstrap-release.sh` do today as one script;
/// here it's one function calling each module's `run_with()` in turn so any
/// individual step stays independently testable and independently
/// skippable (`config::Config`'s skip flags gate each call).
///
/// `services::ensure_nodestore` runs before `targets` -- `targets/
/// upstream.rs` orders `kube-apiserver.service` `After=nodestore.service`,
/// so nodestore has to actually be installed and enabled first.
/// `nodescheduler`/`nodecontroller` run right after `targets` -- they
/// *are* this project's kube-scheduler/kube-controller-manager
/// replacements (`targets/upstream.rs` only installs `kube-apiserver` now;
/// see that module's and `services.rs`'s doc comments), so they take the
/// slot the upstream binaries used to occupy. `cni` deliberately runs
/// **after** those, not before: `flanneld`'s kube-subnet-mgr mode needs a
/// live kubeconfig pointed at a reachable apiserver, same ordering
/// constraint `cni.sh`'s own `start_flanneld` comment documents
/// ("flanneld needs a KUBECONFIG (control plane must be up first)").
/// `targets::run_with` itself runs after `pki`/`kubeconfig` (it needs the
/// minted PKI to start the apiserver trusting it) and before
/// `service_reconciler`/`manifests` (both need a reachable apiserver to
/// apply against). **`rbac` runs before `nodescheduler`, not after** --
/// its `apply_nodescheduler_dra_grant` (see `rbac.rs`'s second finding)
/// has to land before `nodescheduler` starts watching `resource.k8s.io`,
/// or its reflector 403s in a retry loop before ever reaching the pod
/// watch. `nodelet`/`nodeproxy` run last, after containerd/CNI/the
/// apiserver/scheduler/controller-manager are all up -- same order
/// `bootstrap-source.sh` uses today.
pub fn run_all() -> Result<()> {
    let cfg = config::Config::from_env()?;
    toolchain::run_with(&cfg)?;
    containerd::run_with(&cfg)?;
    fetch::run_with(&cfg)?;
    pki::run_with(&cfg)?;
    kubeconfig::run_with(&cfg)?;
    services::ensure_nodestore(&cfg)?;
    targets::run_with(&cfg)?;
    rbac::run_with(&cfg)?;
    services::ensure_nodescheduler(&cfg)?;
    services::ensure_nodecontroller(&cfg)?;
    cni::run_with(&cfg)?;
    service_reconciler::run_with(&cfg)?;
    manifests::run_with(&cfg)?;
    services::ensure_nodelet(&cfg)?;
    services::ensure_nodeproxy(&cfg)?;
    Ok(())
}
