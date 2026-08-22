//! Build-from-source vs. fetch-a-release, and layout selection — replaces
//! the prebuilt/layout logic spread across `bootstrap-source.sh` and
//! `bootstrap-release.sh` (`NOTK8S_*_PREBUILT` env vars, `--layout=`,
//! tagged vs. latest release resolution).
//!
//! **Scope cut, deliberate:** `Source::Compile` (version-stamp + `cargo
//! build` per `components.rs`'s table, honoring `Config::layout`) is real.
//! `Source::Release` (resolve `Config::release_tag` against GitHub Releases,
//! download the matching asset) is **not yet ported** -- asset-name
//! matching across arch x profile x layout is its own chunk of work, queued
//! next. `nodelet-build.sh`'s low-RAM-host LTO/`CARGO_BUILD_JOBS=1`
//! fallback is also not yet ported here; a from-source build on a
//! constrained host should set those env vars itself until it is (see
//! `CLAUDE.md`'s "Memory-constrained build hosts").

use anyhow::{Context, Result};

use crate::components::COMPONENTS;
use crate::config::{Config, Layout, Source};

/// This repo's own GitHub coordinates -- used only to fetch `VERSION` off
/// the `version` branch (`stamp_version_from_release_branch`) and, once
/// ported, to resolve release assets.
const REPO: &str = "centerionware/not-k8s";

pub fn run() -> Result<()> {
    run_with(&Config::from_env()?)
}

pub fn run_with(cfg: &Config) -> Result<()> {
    match cfg.source {
        Source::Compile => build_from_source(cfg),
        Source::Release => anyhow::bail!(
            "nodebootstrap::fetch's Source::Release path is not yet ported -- see this module's \
             doc comment. Use Source::Compile (the default) until then."
        ),
    }
}

fn build_from_source(cfg: &Config) -> Result<()> {
    let repo_root = find_repo_root().context("locating the not-k8s repo root (walking up from CWD for a Cargo.toml with [workspace])")?;
    stamp_version_from_release_branch(&repo_root)?;

    match cfg.layout {
        Layout::Split => {
            for component in COMPONENTS {
                cargo_build(&repo_root, &["-p", component.cargo_package])?;
            }
        }
        Layout::Combined => {
            // crates/notk8s links every component crate behind its own
            // Cargo feature (see CLAUDE.md's "Two build layouts") --
            // building it with no explicit -F takes its default features,
            // which is every component, matching what a release build
            // ships regardless of what an individual device wants (same
            // caveat components.sh's want_* predicates document).
            cargo_build(&repo_root, &["-p", "notk8s"])?;
        }
    }
    Ok(())
}

fn cargo_build(repo_root: &std::path::Path, extra_args: &[&str]) -> Result<()> {
    tracing::info!(args = ?extra_args, "cargo build");
    let status = std::process::Command::new("cargo")
        .arg("build")
        .arg("--release")
        .args(extra_args)
        .current_dir(repo_root)
        .status()
        .context("running cargo build")?;
    anyhow::ensure!(status.success(), "cargo build {} failed", extra_args.join(" "));
    Ok(())
}

/// Walks up from the current directory looking for a `Cargo.toml` whose
/// first bytes declare `[workspace]` -- this crate's own workspace root,
/// same file `docs/NODEBOOTSTRAP_PLAN.md`'s tree calls out. Doesn't shell
/// out to `cargo locate-project` because that requires cargo to already be
/// on PATH, which `toolchain::ensure_rust` may not have finished ensuring
/// yet at the point `fetch` needs this.
fn find_repo_root() -> Result<std::path::PathBuf> {
    let mut dir = std::env::current_dir().context("reading CWD")?;
    loop {
        let candidate = dir.join("Cargo.toml");
        if let Ok(contents) = std::fs::read_to_string(&candidate) {
            if contents.contains("[workspace]") {
                return Ok(dir);
            }
        }
        if !dir.pop() {
            anyhow::bail!(
                "no workspace Cargo.toml found walking up from {} -- run nodebootstrap from \
                 inside a not-k8s checkout, or set NODEBOOTSTRAP_REPO_ROOT",
                std::env::current_dir().map(|d| d.display().to_string()).unwrap_or_default()
            );
        }
    }
}

/// Reads `VERSION` off the `version` branch (over plain HTTPS -- `git`/`gh`
/// aren't assumed present on a bootstrap target) and stamps it into the
/// workspace `Cargo.toml`'s `[workspace.package] version` field, so a
/// from-source build reports the same version a release binary would carry
/// instead of the placeholder every crate inherits from that field today.
/// Read-only against the `version` branch -- bumping it stays
/// `version-bump.sh`'s job, run only by `release.yml`'s `publish-release`
/// stage.
fn stamp_version_from_release_branch(repo_root: &std::path::Path) -> Result<()> {
    let url = format!("https://raw.githubusercontent.com/{REPO}/version/VERSION");
    let dest = std::env::temp_dir().join("nodebootstrap-VERSION");
    crate::pkg::fetch_url(&url, &dest).context("fetching VERSION off the version branch")?;
    let version = std::fs::read_to_string(&dest).context("reading fetched VERSION file")?;
    let version = version.trim();
    anyhow::ensure!(
        version.split('.').count() == 3 && version.split('.').all(|p| p.parse::<u32>().is_ok()),
        "VERSION file off the version branch doesn't look like MAJOR.MINOR.PATCH: '{version}'"
    );

    let cargo_toml_path = repo_root.join("Cargo.toml");
    let cargo_toml = std::fs::read_to_string(&cargo_toml_path).context("reading workspace Cargo.toml")?;
    let stamped = stamp_version(&cargo_toml, version)
        .with_context(|| format!("no [workspace.package] version field found in {}", cargo_toml_path.display()))?;
    std::fs::write(&cargo_toml_path, stamped).context("writing stamped Cargo.toml")?;
    tracing::info!(version, "stamped workspace version from the version branch");
    Ok(())
}

/// Replaces the first `version = "..."` line's value with `new_version`.
/// The workspace `Cargo.toml`'s only `version = "..."` line lives under
/// `[workspace.package]` (confirmed against this repo's own file), so a
/// plain first-match replace is correct here without a TOML parser.
fn stamp_version(cargo_toml: &str, new_version: &str) -> Option<String> {
    let mut replaced = false;
    let out: String = cargo_toml
        .lines()
        .map(|line| {
            if !replaced && line.trim_start().starts_with("version") && line.contains('=') {
                replaced = true;
                format!("version = \"{new_version}\"")
            } else {
                line.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    replaced.then_some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stamp_version_replaces_only_the_workspace_package_version() {
        let toml = "[workspace]\nmembers = [\"a\"]\n\n[workspace.package]\nversion = \"0.1.0\"\nedition = \"2021\"\n";
        let stamped = stamp_version(toml, "1.2.3").expect("found a version field");
        assert!(stamped.contains("version = \"1.2.3\""));
        assert!(!stamped.contains("0.1.0"));
        assert!(stamped.contains("edition = \"2021\""));
    }

    #[test]
    fn stamp_version_returns_none_when_no_version_field_exists() {
        let toml = "[workspace]\nmembers = [\"a\"]\n";
        assert!(stamp_version(toml, "1.2.3").is_none());
    }
}
