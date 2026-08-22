//! CNI setup — replaces `deploy/lib/cni.sh`.
//!
//! Real logic to land here: install and configure flannel when
//! `Config::cni_provider` is `Some("flannel")` (the default); do nothing
//! and leave the host to whatever CNI it already has (Cilium, etc.) when
//! it's `None`. Same "a stale flanneld survives a re-bootstrap" trap
//! `CLAUDE.md` documents applies here — check the running unit's actual
//! config before deciding "already set up," don't just check the binary
//! exists.

use anyhow::Result;

use crate::config::Config;

pub fn run() -> Result<()> {
    run_with(&Config::from_env()?)
}

pub fn run_with(cfg: &Config) -> Result<()> {
    let Some(provider) = &cfg.cni_provider else {
        tracing::info!("skipping CNI setup (NODEBOOTSTRAP_CNI=none) -- bring-your-own");
        return Ok(());
    };
    if provider != "flannel" {
        anyhow::bail!(
            "nodebootstrap only knows how to install 'flannel' itself; \
             NODEBOOTSTRAP_CNI={provider} means bring-your-own and skip this step \
             (set NODEBOOTSTRAP_CNI=none)"
        );
    }
    anyhow::bail!(
        "nodebootstrap::cni is a scaffold, not yet implemented -- see \
         docs/NODEBOOTSTRAP_PLAN.md Phase 1"
    )
}
