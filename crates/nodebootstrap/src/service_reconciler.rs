//! The `kubernetes` default Service + endpoint reconciler -- what makes
//! `kubernetes.default.svc` resolve to the apiserver's own reachable
//! address/port from inside the cluster. Upstream's
//! `pkg/controlplane/controller/kubernetes` equivalent. Group O's last
//! piece besides manifests.

use anyhow::Result;

use crate::config::Config;

pub fn run() -> Result<()> {
    run_with(&Config::from_env()?)
}

pub fn run_with(cfg: &Config) -> Result<()> {
    if cfg.skip_service_reconciler {
        tracing::info!(
            "skipping kubernetes Service reconciler (NODEBOOTSTRAP_SKIP_SERVICE_RECONCILER)"
        );
        return Ok(());
    }
    anyhow::bail!(
        "nodebootstrap::service_reconciler is a scaffold, not yet implemented -- see \
         docs/NODEBOOTSTRAP_PLAN.md Phase 1"
    )
}
