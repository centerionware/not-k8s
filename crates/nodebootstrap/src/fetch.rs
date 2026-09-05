//! Build-from-source vs. fetch-a-release, and layout selection — replaces
//! the prebuilt/layout logic spread across `bootstrap-source.sh` and
//! `bootstrap-release.sh` (`NOTK8S_*_PREBUILT` env vars, `--layout=`,
//! tagged vs. latest release resolution).
//!
//! Both `Source::Compile` (version-stamp + `cargo build` per
//! `components.rs`'s table, honoring `Config::layout`) and `Source::Release`
//! (resolve `Config::release_tag` against GitHub Releases, download the
//! matching asset) are real. `nodelet-build.sh`'s low-RAM-host LTO/
//! `CARGO_BUILD_JOBS=1` fallback used by the shell builder is also applied
//! here so a device can rebuild itself without exhausting its memory.

use anyhow::{Context, Result};

use crate::components::COMPONENTS;
use crate::config::{BuildProfile, Config, Layout, Source};
use crate::toolchain;

/// This repo's own GitHub coordinates -- used to fetch `VERSION` off the
/// `version` branch (`stamp_version_from_release_branch`) and to resolve
/// release assets (`download_release`).
const REPO: &str = "centerionware/not-k8s";

pub fn run() -> Result<()> {
    run_with(&Config::from_env()?)
}

pub fn run_with(cfg: &Config) -> Result<()> {
    if try_prebuilt(cfg)? {
        return Ok(());
    }
    match cfg.source {
        Source::Compile => build_from_source(cfg),
        Source::Release => download_release(cfg),
    }
}

/// Whether the current process supplied a prebuilt binary seam. This is
/// checked before toolchain setup so prebuilt CI/device installs do not
/// require Rust or protoc.
pub fn has_prebuilt() -> bool {
    std::env::var_os("NOTK8S_COMBINED_PREBUILT").is_some()
        || std::env::var_os("NODEBOOTSTRAP_COMBINED_SELF").is_some()
        || COMPONENTS.iter().any(|component| std::env::var_os(component.prebuilt_env).is_some())
}

/// Checks `NOTK8S_COMBINED_PREBUILT`/`NOTK8S_<COMPONENT>_PREBUILT` before
/// ever touching `cfg.source` -- same precedence
/// `nodelet-build.sh`'s `build_nodelet()` documents at its own top ("checks
/// $NOTK8S_COMBINED_PREBUILT before ever touching $SOURCE"). This is the
/// prebuilt seam `CLAUDE.md`'s merge protocol and `release.yml`'s e2e stage
/// both depend on: a binary built once in CI gets staged into every shard
/// (or a local device under test) without a second compile.
///
/// Returns `Ok(true)` when a prebuilt was staged (the caller should not
/// also compile/download), `Ok(false)` when none of these env vars were
/// set at all.
fn try_prebuilt(cfg: &Config) -> Result<bool> {
    let dest_dir = cfg.toolchain_dir().join("bin");

    let combined = std::env::var("NOTK8S_COMBINED_PREBUILT")
        .or_else(|_| std::env::var("NODEBOOTSTRAP_COMBINED_SELF"));
    if let Ok(combined) = combined {
        anyhow::ensure!(
            matches!(cfg.layout, Layout::Combined),
            "NOTK8S_COMBINED_PREBUILT is set (a single binary containing every component), but \
             this run's layout is 'split' (NOTK8S_BUILD_LAYOUT). Set NOTK8S_BUILD_LAYOUT=combined \
             to install it as intended, or supply the per-component prebuilts \
             (NOTK8S_NODELET_PREBUILT/...) instead."
        );
        std::fs::create_dir_all(&dest_dir).context("creating toolchain bin dir")?;
        let dest = dest_dir.join("notk8s");
        install_prebuilt(&combined, &dest)?;
        for component in COMPONENTS {
            let link = dest_dir.join(component.name);
            let _ = std::fs::remove_file(&link);
            #[cfg(unix)]
            std::os::unix::fs::symlink("notk8s", &link).with_context(|| format!("symlinking {}", link.display()))?;
        }
        return Ok(true);
    }

    let supplied: Vec<(&'static crate::components::ComponentSpec, String)> =
        COMPONENTS.iter().filter_map(|c| std::env::var(c.prebuilt_env).ok().map(|path| (c, path))).collect();
    if supplied.is_empty() {
        return Ok(false);
    }
    anyhow::ensure!(
        matches!(cfg.layout, Layout::Split),
        "per-component prebuilt binaries were supplied (NOTK8S_*_PREBUILT), but this run's layout \
         is 'combined' (NOTK8S_BUILD_LAYOUT). A combined binary has to be built/fetched as one -- \
         set NOTK8S_COMBINED_PREBUILT instead, or drop NOTK8S_BUILD_LAYOUT=combined to install the \
         per-component binaries you already have."
    );
    anyhow::ensure!(
        supplied.len() == COMPONENTS.len(),
        "only {}/{} components had a NOTK8S_*_PREBUILT set ({}) -- mixing prebuilt and \
         from-source/from-release components isn't supported (a partial set is far more likely to \
         be an oversight than a request). Set every component's prebuilt env var, or set \
         NOTK8S_COMBINED_PREBUILT for a single binary containing everything.",
        supplied.len(),
        COMPONENTS.len(),
        supplied.iter().map(|(c, _)| c.name).collect::<Vec<_>>().join(", ")
    );
    std::fs::create_dir_all(&dest_dir).context("creating toolchain bin dir")?;
    for (component, path) in &supplied {
        install_prebuilt(path, &dest_dir.join(component.name))?;
    }
    Ok(true)
}

fn install_prebuilt(src: &str, dest: &std::path::Path) -> Result<()> {
    anyhow::ensure!(std::path::Path::new(src).is_file(), "prebuilt binary path doesn't exist or isn't a file: {src}");
    install_binary(std::path::Path::new(src), dest)
        .with_context(|| format!("staging prebuilt {src} to {}", dest.display()))?;
    tracing::info!(src, dest = %dest.display(), "staged prebuilt binary");
    Ok(())
}

/// Replace an installed executable without truncating the file currently used
/// by a running service. Linux rejects opening an active executable for
/// replacement with `ETXTBSY`; a same-directory temporary copy followed by
/// rename gives the service the old inode until its next restart and makes the
/// new inode available atomically.
fn install_binary(src: &std::path::Path, dest: &std::path::Path) -> Result<()> {
    let name = dest.file_name().and_then(|name| name.to_str()).unwrap_or("binary");
    let temporary = dest.with_file_name(format!(".{name}.tmp-{}", std::process::id()));
    let _ = std::fs::remove_file(&temporary);
    std::fs::copy(src, &temporary).with_context(|| format!("copying {} to {}", src.display(), temporary.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&temporary, std::fs::Permissions::from_mode(0o755))
            .with_context(|| format!("making {} executable", temporary.display()))?;
    }
    std::fs::rename(&temporary, dest)
        .with_context(|| format!("installing {} as {}", temporary.display(), dest.display()))?;
    Ok(())
}

fn build_from_source(cfg: &Config) -> Result<()> {
    let repo_root = find_repo_root().context("locating the not-k8s repo root (walking up from CWD for a Cargo.toml with [workspace])")?;
    stamp_version_from_release_branch(&repo_root)?;

    let dest_dir = cfg.toolchain_dir().join("bin");
    std::fs::create_dir_all(&dest_dir).context("creating toolchain bin dir")?;
    let target = toolchain::rust_target(&cfg.arch()).with_context(|| format!("no supported static musl Rust target for arch '{}'", cfg.arch()))?;
    let target_profile = repo_root
        .join("target")
        .join(target)
        .join(match cfg.build_profile {
            BuildProfile::Debug => "debug",
            BuildProfile::Release => "release",
        });

    // Stages every built binary into Config::toolchain_dir()/bin -- the one
    // canonical location `services.rs`'s installers look for a component
    // binary, regardless of whether it got there via a from-source build or
    // download_release() below. Mirrors bootstrap-source.sh's own
    // "copy to bin/, don't leave it in target/" step.
    let stage = |bin: &str| -> Result<()> {
        let src = target_profile.join(bin);
        let dest = dest_dir.join(bin);
        install_binary(&src, &dest).with_context(|| format!("staging {} to {}", src.display(), dest.display()))?;
        Ok(())
    };

    if matches!(cfg.layout, Layout::Split | Layout::Both) {
        for component in COMPONENTS {
            cargo_build(cfg, &repo_root, &["-p", component.cargo_package])?;
            stage(component.name)?;
        }
    }
    if matches!(cfg.layout, Layout::Combined | Layout::Both) {
        cargo_build(cfg, &repo_root, &["-p", "notk8s"])?;
        stage("notk8s")?;
        // `both` leaves the split binaries installed and keeps the combined
        // binary alongside them for comparison/packaging.
        if matches!(cfg.layout, Layout::Combined) {
            for component in COMPONENTS {
                let link = dest_dir.join(component.name);
                let _ = std::fs::remove_file(&link);
                #[cfg(unix)]
                std::os::unix::fs::symlink("notk8s", &link)
                    .with_context(|| format!("symlinking {}", link.display()))?;
            }
        }
    }
    if matches!(cfg.layout, Layout::Combined) {
        // The default combined target already links the installer applet.
        // Do not pay for a second fat-LTO executable with the same code.
        let link = dest_dir.join("nodebootstrap");
        let _ = std::fs::remove_file(&link);
        #[cfg(unix)]
        std::os::unix::fs::symlink("notk8s", &link)
            .with_context(|| format!("symlinking {}", link.display()))?;
    } else {
        cargo_build(cfg, &repo_root, &["-p", "nodebootstrap"])?;
        stage("nodebootstrap")?;
    }
    Ok(())
}

/// Real upstream release asset names, confirmed against this repo's own
/// published releases (`gh api repos/centerionware/not-k8s/releases/latest`):
/// `<bin>-<version-without-v>-linux-<arch>-<profile>`, e.g.
/// `nodelet-0.6.2-linux-x86_64-release`. `<arch>` matches `Config::arch()`'s
/// own `uname -m` vocabulary directly (`x86_64`/`aarch64`/`armv7l`) -- no
/// translation table needed here, unlike `k8s_dl_arch`/`cni_go_arch`
/// elsewhere in this crate, because this repo's own release matrix already
/// uses that vocabulary (`release.yml`'s own `matrix.arch`).
///
/// Builds the selected Cargo profile. Release is the default for a real
/// device install; CI can select debug with `NOTK8S_BUILD_PROFILE=debug` for
/// a faster e2e bootstrap, then invoke this command again for release assets.
fn download_release(cfg: &Config) -> Result<()> {
    let tag = resolve_release_tag(cfg.release_tag.as_deref())?;
    let version = tag.strip_prefix('v').unwrap_or(&tag);
    let assets = list_release_assets(&tag)?;
    let arch = cfg.arch();

    let mut wanted: Vec<&str> = match cfg.layout {
        Layout::Split => COMPONENTS.iter().map(|c| c.name).collect(),
        Layout::Combined => vec!["notk8s"],
        Layout::Both => {
            let mut bins: Vec<&str> = COMPONENTS.iter().map(|c| c.name).collect();
            bins.push("notk8s");
            bins
        },
    };
    wanted.push("nodebootstrap");

    let dest_dir = cfg.toolchain_dir().join("bin");
    std::fs::create_dir_all(&dest_dir).context("creating toolchain bin dir")?;

    for bin in wanted {
        let asset_name = format!("{bin}-{version}-linux-{arch}-release");
        let url = assets.get(asset_name.as_str()).with_context(|| {
            format!(
                "release {tag} has no asset named '{asset_name}' -- check \
                 https://github.com/{REPO}/releases/tag/{tag} for what's actually attached"
            )
        })?;
        let dest = dest_dir.join(bin);
        tracing::info!(asset = asset_name, "downloading release asset");
        crate::pkg::fetch_url(url, &dest).with_context(|| format!("fetching {asset_name}"))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(0o755))
                .with_context(|| format!("making {} executable", dest.display()))?;
        }
    }

    // Combined layout: symlink every component name to the one downloaded
    // `notk8s` binary, same as build_from_source() does for a from-source
    // combined build -- `services.rs`'s installers look for a component by
    // name in this same dir regardless of layout or source.
    if matches!(cfg.layout, Layout::Combined) {
        for component in COMPONENTS {
            let link = dest_dir.join(component.name);
            let _ = std::fs::remove_file(&link);
            #[cfg(unix)]
            std::os::unix::fs::symlink("notk8s", &link)
                .with_context(|| format!("symlinking {}", link.display()))?;
        }
    }
    Ok(())
}

/// `None` -> resolve `/releases/latest`; `Some(t)` -> use `t` as given,
/// prefixed with `v` if it doesn't already have one (accepts both
/// `NODEBOOTSTRAP_RELEASE_TAG=1.2.3` and `=v1.2.3`).
fn resolve_release_tag(tag: Option<&str>) -> Result<String> {
    if let Some(t) = tag {
        return Ok(if t.starts_with('v') { t.to_string() } else { format!("v{t}") });
    }
    let url = format!("https://api.github.com/repos/{REPO}/releases/latest");
    let body = github_api_get(&url)?;
    body.get("tag_name")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .with_context(|| format!("no tag_name in the response from {url}"))
}

/// Maps asset name -> `browser_download_url` for every asset on `tag`'s
/// release.
fn list_release_assets(tag: &str) -> Result<std::collections::HashMap<String, String>> {
    let url = format!("https://api.github.com/repos/{REPO}/releases/tags/{tag}");
    let body = github_api_get(&url)?;
    let assets = body.get("assets").and_then(|v| v.as_array()).with_context(|| format!("no assets array in the response from {url}"))?;
    let mut map = std::collections::HashMap::new();
    for asset in assets {
        if let (Some(name), Some(download_url)) =
            (asset.get("name").and_then(|v| v.as_str()), asset.get("browser_download_url").and_then(|v| v.as_str()))
        {
            map.insert(name.to_string(), download_url.to_string());
        }
    }
    Ok(map)
}

/// GitHub's REST API requires a `User-Agent` header (returns `403` without
/// one) -- the one thing `pkg::fetch_url`'s plain GET doesn't set, so this
/// stays a small local helper rather than a `pkg.rs` addition every other
/// caller would carry for no reason.
fn github_api_get(url: &str) -> Result<serde_json::Value> {
    let response = ureq::get(url)
        .set("User-Agent", "not-k8s-nodebootstrap")
        .set("Accept", "application/vnd.github+json")
        .call()
        .with_context(|| format!("GET {url}"))?;
    let body = response.into_string().with_context(|| format!("reading response body from {url}"))?;
    serde_json::from_str(&body).with_context(|| format!("parsing JSON from {url}"))
}

fn cargo_build(cfg: &Config, repo_root: &std::path::Path, extra_args: &[&str]) -> Result<()> {
    let package = extra_args.iter().position(|arg| *arg == "-p").and_then(|index| extra_args.get(index + 1)).copied();
    tracing::info!(args = ?extra_args, with_cri = cfg.with_cri, "cargo build");
    let mut command = std::process::Command::new("cargo");
    command.arg("build");
    if cfg.build_profile == BuildProfile::Release {
        command.arg("--release");
        // This is the on-device source-build path. Keep its guarantees
        // explicit at the command boundary even if the workspace release
        // profile changes: deployed binaries stay small, fast, and static.
        command.env("CARGO_PROFILE_RELEASE_OPT_LEVEL", "s");
        command.env("CARGO_PROFILE_RELEASE_LTO", "fat");
        command.env("CARGO_PROFILE_RELEASE_CODEGEN_UNITS", "1");
        command.env("CARGO_PROFILE_RELEASE_STRIP", "symbols");
        command.env("CARGO_PROFILE_RELEASE_PANIC", "abort");
    }
    command.arg("--target").arg(toolchain::rust_target(&cfg.arch()).context("no supported static musl Rust target")?).args(extra_args);
    if cfg.toolchain_dir().join("cargo/bin/cargo").is_file() {
        command.env("RUSTUP_HOME", cfg.toolchain_dir().join("rustup"));
        command.env("CARGO_HOME", cfg.toolchain_dir().join("cargo"));
    }
    if cfg.with_cri && matches!(package, Some("nodelet" | "notk8s")) {
        command.args(["--features", "cri"]);
    }
    // Keep the installer itself on the full size/speed profile even on a
    // small device. The surrounding runtime builds may use the documented
    // thin-LTO fallback there, but the binary that will be invoked on every
    // future bootstrap is worth the one full optimized build.
    if cfg.build_profile == BuildProfile::Release && low_memory_host() && package != Some("nodebootstrap") {
        command.env("CARGO_BUILD_JOBS", "1");
        command.env("CARGO_PROFILE_RELEASE_LTO", "thin");
        command.env("CARGO_PROFILE_RELEASE_CODEGEN_UNITS", "16");
        tracing::info!("low-memory host detected; using a serialized thin-LTO release build");
    }
    let status = command.current_dir(repo_root).status().context("running cargo build")?;
    anyhow::ensure!(status.success(), "cargo build {} failed", extra_args.join(" "));
    Ok(())
}

fn low_memory_host() -> bool {
    std::fs::read_to_string("/proc/meminfo")
        .ok()
        .and_then(|contents| contents.lines().find_map(|line| line.strip_prefix("MemTotal:")?.split_whitespace().next()?.parse::<u64>().ok()))
        .is_some_and(|kilobytes| kilobytes < 4 * 1024 * 1024)
}

/// Walks up from the current directory looking for a `Cargo.toml` whose
/// first bytes declare `[workspace]` -- this crate's own workspace root,
/// same file `docs/NODEBOOTSTRAP_PLAN.md`'s tree calls out. Doesn't shell
/// out to `cargo locate-project` because that requires cargo to already be
/// on PATH, which `toolchain::ensure_rust` may not have finished ensuring
/// yet at the point `fetch` needs this.
fn find_repo_root() -> Result<std::path::PathBuf> {
    if let Ok(root) = std::env::var("NODEBOOTSTRAP_REPO_ROOT") {
        let path = std::path::PathBuf::from(root);
        anyhow::ensure!(path.join("Cargo.toml").is_file(), "NODEBOOTSTRAP_REPO_ROOT does not contain Cargo.toml: {}", path.display());
        return Ok(path);
    }
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
/// Accepts a plain MAJOR.MINOR.PATCH triple or a 4th, hand-added
/// point-release component (MAJOR.MINOR.PATCH.POINT, e.g. `0.7.2.1`) --
/// matching `release.yml`'s version-bump step, which increments whichever
/// component is last regardless of how many there are. Read-only against
/// the `version` branch -- bumping it stays `version-bump.sh`'s job, run
/// only by `release.yml`'s `publish-release` stage.
fn stamp_version_from_release_branch(repo_root: &std::path::Path) -> Result<()> {
    let url = format!("https://raw.githubusercontent.com/{REPO}/version/VERSION");
    let dest = std::env::temp_dir().join("nodebootstrap-VERSION");
    crate::pkg::fetch_url(&url, &dest).context("fetching VERSION off the version branch")?;
    let version = std::fs::read_to_string(&dest).context("reading fetched VERSION file")?;
    let version = version.trim();
    anyhow::ensure!(
        looks_like_version(version),
        "VERSION file off the version branch doesn't look like MAJOR.MINOR.PATCH(.POINT): '{version}'"
    );

    let cargo_toml_path = repo_root.join("Cargo.toml");
    let cargo_toml = std::fs::read_to_string(&cargo_toml_path).context("reading workspace Cargo.toml")?;
    let stamped = stamp_version(&cargo_toml, version)
        .with_context(|| format!("no [workspace.package] version field found in {}", cargo_toml_path.display()))?;
    std::fs::write(&cargo_toml_path, stamped).context("writing stamped Cargo.toml")?;
    tracing::info!(version, "stamped workspace version from the version branch");
    Ok(())
}

/// True for a MAJOR.MINOR.PATCH triple or a 4th, hand-added point-release
/// component (MAJOR.MINOR.PATCH.POINT) -- at least 3 dot-separated numeric
/// parts, matching what `release.yml`'s version-bump step now produces.
fn looks_like_version(version: &str) -> bool {
    let parts: Vec<_> = version.split('.').collect();
    parts.len() >= 3 && parts.iter().all(|p| p.parse::<u32>().is_ok())
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

    #[test]
    fn looks_like_version_accepts_a_plain_triple_and_a_point_release_quad() {
        assert!(looks_like_version("0.7.2"));
        assert!(looks_like_version("0.7.2.1"));
    }

    #[test]
    fn looks_like_version_rejects_non_numeric_or_too_short() {
        assert!(!looks_like_version("0.7"));
        assert!(!looks_like_version("v0.7.2"));
        assert!(!looks_like_version("0.7.x"));
        assert!(!looks_like_version(""));
    }

    #[test]
    fn resolve_release_tag_adds_v_prefix_only_when_missing() {
        assert_eq!(resolve_release_tag(Some("1.2.3")).unwrap(), "v1.2.3");
        assert_eq!(resolve_release_tag(Some("v1.2.3")).unwrap(), "v1.2.3");
    }

    #[test]
    fn asset_name_matches_this_repos_real_release_convention() {
        // Confirmed live against `gh api repos/centerionware/not-k8s/
        // releases/latest`: e.g. "nodelet-0.6.2-linux-x86_64-release".
        let bin = "nodelet";
        let version = "0.6.2";
        let arch = "x86_64";
        let asset_name = format!("{bin}-{version}-linux-{arch}-release");
        assert_eq!(asset_name, "nodelet-0.6.2-linux-x86_64-release");
    }

    #[test]
    fn list_release_assets_parses_a_real_shaped_response() {
        // Trimmed shape of a real `GET /repos/{repo}/releases/tags/{tag}`
        // response -- only the fields list_release_assets reads.
        let body = r#"{
            "tag_name": "v0.6.2",
            "assets": [
                {
                    "name": "nodelet-0.6.2-linux-x86_64-release",
                    "browser_download_url": "https://github.com/centerionware/not-k8s/releases/download/v0.6.2/nodelet-0.6.2-linux-x86_64-release"
                },
                {
                    "name": "deploy.tar.gz",
                    "browser_download_url": "https://github.com/centerionware/not-k8s/releases/download/v0.6.2/deploy.tar.gz"
                }
            ]
        }"#;
        let parsed: serde_json::Value = serde_json::from_str(body).expect("valid JSON");
        let assets: std::collections::HashMap<String, String> = parsed["assets"]
            .as_array()
            .unwrap()
            .iter()
            .map(|a| (a["name"].as_str().unwrap().to_string(), a["browser_download_url"].as_str().unwrap().to_string()))
            .collect();
        assert_eq!(
            assets.get("nodelet-0.6.2-linux-x86_64-release").unwrap(),
            "https://github.com/centerionware/not-k8s/releases/download/v0.6.2/nodelet-0.6.2-linux-x86_64-release"
        );
        assert!(assets.contains_key("deploy.tar.gz"));
    }
}
