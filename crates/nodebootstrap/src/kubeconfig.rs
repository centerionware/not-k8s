//! kubeconfig emission for `kubectl` and every in-cluster component
//! (`nodelet`, `nodeproxy`, `nodescheduler`, `nodecontroller`), driven by
//! the certs `pki.rs` mints. Depends on `pki::run_with` having already
//! produced the CA and per-component client certs.

use anyhow::Result;

use crate::config::Config;

pub fn run() -> Result<()> {
    run_with(&Config::from_env()?)
}

pub fn run_with(cfg: &Config) -> Result<()> {
    if cfg.skip_kubeconfig {
        tracing::info!("skipping kubeconfig emission (NODEBOOTSTRAP_SKIP_KUBECONFIG)");
        return Ok(());
    }
    anyhow::bail!(
        "nodebootstrap::kubeconfig is a scaffold, not yet implemented -- see \
         docs/NODEBOOTSTRAP_PLAN.md Phase 1"
    )
}
