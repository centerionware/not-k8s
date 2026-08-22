//! CoreDNS + flannel manifests, moved into `deploy/` and applied via the
//! generated kubeconfig once the target apiserver is reachable. The other
//! half of Group O besides PKI/RBAC/kubeconfig/Service-reconciler.
//!
//! Flannel manifests only apply when `Config::cni_provider ==
//! Some("flannel")` -- see `cni.rs`. Will split into `manifests/{coredns,
//! flannel}.rs` once real logic lands.

use anyhow::Result;

use crate::config::Config;

pub fn run() -> Result<()> {
    run_with(&Config::from_env()?)
}

pub fn run_with(cfg: &Config) -> Result<()> {
    if cfg.skip_manifests {
        tracing::info!("skipping manifest apply (NODEBOOTSTRAP_SKIP_MANIFESTS)");
        return Ok(());
    }
    anyhow::bail!(
        "nodebootstrap::manifests is a scaffold, not yet implemented -- see \
         docs/NODEBOOTSTRAP_PLAN.md Phase 1"
    )
}
