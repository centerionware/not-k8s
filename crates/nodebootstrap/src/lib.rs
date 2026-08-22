//! Library surface for `nodebootstrap`. See `docs/NODEBOOTSTRAP_PLAN.md` for
//! the module-to-shell-script mapping and the Phase 1 / Phase 2 split.
//!
//! Every module here is a scaffold: it defines the surface the plan doc
//! names and the doc comment explaining what real logic belongs where, but
//! none of it is implemented yet. Fill these in group by group, each its
//! own branch/PR/e2e case per `CLAUDE.md`'s merge protocol — do not land
//! the real logic for more than one module per PR.

pub mod config;
pub mod containerd;
pub mod cni;
pub mod components;
pub mod fetch;
pub mod kubeconfig;
pub mod manifests;
pub mod pki;
pub mod rbac;
pub mod service_reconciler;
pub mod targets;
pub mod toolchain;

use anyhow::Result;

/// Runs every phase in dependency order: toolchain -> containerd -> cni ->
/// fetch -> pki -> kubeconfig -> targets (install/start the apiserver) ->
/// rbac -> service-reconciler -> manifests. This is what
/// `bootstrap-source.sh`/`bootstrap-release.sh` do today as one script;
/// here it's one function calling each module's `run_with()` in turn so any
/// individual step stays independently testable and independently
/// skippable (`config::Config`'s skip flags gate each call).
///
/// `targets::run_with` runs after `pki`/`kubeconfig` (it needs the minted
/// PKI to start the apiserver trusting it) and before `rbac`/
/// `service_reconciler`/`manifests` (all three need a reachable apiserver
/// to apply against).
pub fn run_all() -> Result<()> {
    let cfg = config::Config::from_env()?;
    toolchain::run_with(&cfg)?;
    containerd::run_with(&cfg)?;
    cni::run_with(&cfg)?;
    fetch::run_with(&cfg)?;
    pki::run_with(&cfg)?;
    kubeconfig::run_with(&cfg)?;
    targets::run_with(&cfg)?;
    rbac::run_with(&cfg)?;
    service_reconciler::run_with(&cfg)?;
    manifests::run_with(&cfg)?;
    Ok(())
}
