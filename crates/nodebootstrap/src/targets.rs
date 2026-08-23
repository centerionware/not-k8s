//! The seam that makes `docs/NODEBOOTSTRAP_PLAN.md` point 3 real: install
//! and start whichever apiserver/controller-manager/scheduler combination
//! `Config::target` names, using the PKI/kubeconfig `pki.rs`/
//! `kubeconfig.rs` already produced. Everything else in this crate
//! (`rbac.rs`, `service_reconciler.rs`, `manifests.rs`) only needs *a*
//! reachable, spec-compliant apiserver -- it doesn't know or care which one
//! this module started.
//!
//! `main`'s only implementation is `upstream` (real `kube-apiserver` +
//! `kube-controller-manager` + `kube-scheduler` against `nodestore`, the
//! same binaries `deploy/lib/upstream-kube-apiserver.sh` and its siblings
//! already fetch -- but pointed at PKI this crate minted, not borrowed from
//! k3s). A `nodeapiserver` implementation is added here, and made the
//! default, only on the `nodeapiserver` integration branch once that
//! component's own acceptance criteria in `APISERVER_PLAN.md` are met.

pub mod upstream;

use anyhow::Result;

use crate::config::{Config, Target};

pub fn run_with(cfg: &Config) -> Result<()> {
    match cfg.target {
        Target::Upstream => upstream::run_with(cfg),
    }
}

/// Complete the target-specific post-nodelet handoff, if this target needs
/// one. Keeping the dispatch here preserves the target seam for the future
/// nodeapiserver implementation.
pub fn enable_nodelet_proxy(cfg: &Config) -> Result<()> {
    match cfg.target {
        Target::Upstream => upstream::enable_nodelet_proxy(cfg),
    }
}
