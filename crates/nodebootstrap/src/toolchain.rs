//! Toolchain presence checks — replaces `deploy/lib/toolchain-{rust,c,go,protoc}.sh`.
//!
//! Real logic to land here: detect an existing `rustc`/`cargo`/`protoc`/`go`
//! on PATH at the version each build needs, install the missing ones (same
//! package-manager detection the shell scripts already do: apt/apk/dnf/...),
//! and — per `CLAUDE.md`'s memory-constrained-host notes — surface the
//! `CARGO_BUILD_JOBS=1` / low-RAM LTO fallback decision `nodelet-build.sh`
//! makes today, so a Rust caller gets the same protection a shell caller
//! does rather than a silent OOM.

use anyhow::Result;

use crate::config::Config;

pub fn run() -> Result<()> {
    run_with(&Config::from_env()?)
}

pub fn run_with(cfg: &Config) -> Result<()> {
    if cfg.skip_toolchain {
        tracing::info!("skipping toolchain setup (NODEBOOTSTRAP_SKIP_TOOLCHAIN)");
        return Ok(());
    }
    anyhow::bail!(
        "nodebootstrap::toolchain is a scaffold, not yet implemented -- see \
         docs/NODEBOOTSTRAP_PLAN.md Phase 1"
    )
}
