//! Build-from-source vs. fetch-a-release, and layout selection — replaces
//! the prebuilt/layout logic spread across `bootstrap-source.sh` and
//! `bootstrap-release.sh` (`NOTK8S_*_PREBUILT` env vars, `--layout=`,
//! tagged vs. latest release resolution).
//!
//! Real logic to land here: `Source::Compile` shells out to `cargo build`
//! per `components.rs`'s table (mirroring `deploy/lib/components.sh` and
//! `deploy/lib/nodelet-build.sh`'s low-RAM-host fallback); `Source::Release`
//! resolves `Config::release_tag` (`None` = latest) against GitHub Releases
//! and downloads the matching asset for `Config::layout` (combined
//! `notk8s` single asset, or the per-component split assets) and the host
//! arch.
//!
//! **Version stamping on a source build**: before invoking `cargo build`,
//! `stamp_version_from_release_branch()` must fetch `VERSION` from the
//! `version` branch (same file `deploy/lib/version-bump.sh` reads/bumps at
//! release time -- see `CLAUDE.md`'s "Version tracking") and write it into
//! the workspace `Cargo.toml`'s `[workspace.package] version` before
//! building, so a from-source build reports the same version a release
//! binary would carry rather than the stale `0.1.0` placeholder every crate
//! inherits from it today. Read-only against `version` -- this never
//! commits a bump itself; that stays `version-bump.sh`'s job, run only by
//! `release.yml`'s `publish-release` stage.

use anyhow::Result;

use crate::config::{Config, Source};

pub fn run() -> Result<()> {
    run_with(&Config::from_env()?)
}

pub fn run_with(cfg: &Config) -> Result<()> {
    if cfg.source == Source::Compile {
        stamp_version_from_release_branch()?;
    }
    anyhow::bail!(
        "nodebootstrap::fetch is a scaffold, not yet implemented -- see \
         docs/NODEBOOTSTRAP_PLAN.md Phase 1"
    )
}

/// Reads `VERSION` off the `version` branch and stamps it into the
/// workspace `Cargo.toml` before a from-source build. Stub -- see the
/// module doc comment above for what real logic belongs here.
fn stamp_version_from_release_branch() -> Result<()> {
    anyhow::bail!(
        "nodebootstrap::fetch::stamp_version_from_release_branch is a scaffold, not yet \
         implemented -- see docs/NODEBOOTSTRAP_PLAN.md Phase 1"
    )
}
