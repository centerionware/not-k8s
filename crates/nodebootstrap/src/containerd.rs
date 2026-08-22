//! containerd + runc + CNI plugin install/verify — replaces
//! `deploy/lib/container-runtime.sh`.
//!
//! Real logic to land here: detect an already-installed, already-running
//! containerd; if absent, fetch and install it plus `runc` and the CNI
//! plugin binaries for the host's arch. Per `CLAUDE.md`'s "Known CI
//! gotcha", also needs the `disabled_plugins` CRI-plugin strip-and-restart
//! `container-runtime.sh` already does for `ubuntu-latest` runners whose
//! bundled containerd ships CRI disabled.

use anyhow::Result;

use crate::config::Config;

pub fn run() -> Result<()> {
    run_with(&Config::from_env()?)
}

pub fn run_with(cfg: &Config) -> Result<()> {
    if cfg.skip_containerd {
        tracing::info!("skipping containerd setup (NODEBOOTSTRAP_SKIP_CONTAINERD)");
        return Ok(());
    }
    anyhow::bail!(
        "nodebootstrap::containerd is a scaffold, not yet implemented -- see \
         docs/NODEBOOTSTRAP_PLAN.md Phase 1"
    )
}
