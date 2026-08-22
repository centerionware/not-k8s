//! Toolchain presence checks — replaces `deploy/lib/toolchain-{rust,c,go,protoc}.sh`.
//!
//! **Scope cut, deliberate:** ports each script's primary path (already-
//! present check -> package manager -> official prebuilt release). Does
//! **not** yet port the deepest from-source fallback tiers (`build_gcc_
//! from_source`, `build_go_from_source`, `try_musl_cc_toolchain`,
//! `build_protoc_from_source`) -- those are slow (30-90+ min), rare in
//! practice (CI builds centrally; see each function's `bail!` for why), and
//! each is 30-90 lines of its own. Queued as follow-up, not dropped.
//! `CARGO_BUILD_JOBS=1` / low-RAM LTO fallback protection
//! (`nodelet-build.sh`'s job, not this file's) is unaffected either way.

use anyhow::{Context, Result};

use crate::config::Config;
use crate::pkg::{fetch_url, pkg_install, PkgNames};

pub fn run() -> Result<()> {
    run_with(&Config::from_env()?)
}

pub fn run_with(cfg: &Config) -> Result<()> {
    if cfg.skip_toolchain {
        tracing::info!("skipping toolchain setup (NODEBOOTSTRAP_SKIP_TOOLCHAIN)");
        return Ok(());
    }
    ensure_rust(cfg)?;
    ensure_protoc(cfg)?;
    Ok(())
}

fn command_version_output(bin: &str, arg: &str) -> Option<String> {
    std::process::Command::new(bin)
        .arg(arg)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).lines().next().unwrap_or_default().to_string())
}

/// This workspace's actual MSRV (kube 4.0.0 / tonic 0.14.6) -- see
/// `toolchain-rust.sh`'s own comment on when to bump this: if `cargo
/// build` ever fails with "rustc X is not supported" despite this check
/// passing, a dependency bump raised the MSRV past what this knows about.
const MIN_CARGO_MINOR: u32 = 88;

fn cargo_is_new_enough() -> bool {
    let Some(out) = command_version_output("cargo", "--version") else { return false };
    // "cargo 1.90.0 (...)" -> 90
    out.split_whitespace()
        .nth(1)
        .and_then(|v| v.split('.').nth(1))
        .and_then(|minor| minor.parse::<u32>().ok())
        .is_some_and(|minor| minor >= MIN_CARGO_MINOR)
}

fn rustup_target(arch: &str) -> Option<&'static str> {
    Some(match arch {
        "x86_64" => "x86_64-unknown-linux-gnu",
        "aarch64" => "aarch64-unknown-linux-gnu",
        "armv7l" => "armv7-unknown-linux-gnueabihf",
        "armv6l" => "arm-unknown-linux-gnueabihf",
        "i686" => "i686-unknown-linux-gnu",
        "riscv64" => "riscv64gc-unknown-linux-gnu",
        "ppc64le" => "powerpc64le-unknown-linux-gnu",
        "s390x" => "s390x-unknown-linux-gnu",
        "loongarch64" => "loongarch64-unknown-linux-gnu",
        _ => return None,
    })
}

pub fn ensure_rust(cfg: &Config) -> Result<()> {
    if cargo_is_new_enough() {
        tracing::info!(
            version = command_version_output("cargo", "--version").unwrap_or_default(),
            "Rust present and new enough"
        );
        return Ok(());
    }
    if let Some(v) = command_version_output("cargo", "--version") {
        tracing::warn!("found {v} but this project needs >=1.{MIN_CARGO_MINOR} -- looking for a newer one");
    }

    let names = PkgNames { apt: "cargo rustc", dnf: "cargo rustc", pacman: "rust", apk: "cargo", zypper: "cargo rustc", xbps: "rust" };
    if pkg_install("rust", &names)? && cargo_is_new_enough() {
        tracing::info!("Rust installed via the system package manager");
        return Ok(());
    }

    let arch = cfg.arch();
    let target = rustup_target(&arch).with_context(|| {
        format!(
            "no known rustup target for arch '{arch}' -- no way to build rustc from nothing but a \
             C compiler for an unsupported architecture; the only real path is mrustc \
             (https://github.com/thepowersgang/mrustc), out of scope here"
        )
    })?;

    tracing::info!(target, "installing Rust via rustup");
    let src_dir = cfg.src_dir();
    std::fs::create_dir_all(&src_dir).context("creating scratch dir for rustup-init.sh")?;
    let rustup_init = src_dir.join("rustup-init.sh");
    fetch_url("https://sh.rustup.rs", &rustup_init).context("fetching rustup-init.sh")?;
    let toolchain_dir = cfg.toolchain_dir();
    let status = std::process::Command::new("sh")
        .arg(&rustup_init)
        .args(["-y", "--default-toolchain", "stable", "--target", target, "--no-modify-path"])
        .env("RUSTUP_HOME", toolchain_dir.join("rustup"))
        .env("CARGO_HOME", toolchain_dir.join("cargo"))
        .status()
        .context("running rustup-init.sh")?;
    anyhow::ensure!(
        status.success(),
        "rustup could not install a toolchain for {target} -- this architecture has no official \
         prebuilt rustc"
    );
    tracing::info!("Rust installed via rustup into {}", toolchain_dir.join("cargo/bin").display());
    Ok(())
}

fn protoc_arch(arch: &str) -> Option<&'static str> {
    Some(match arch {
        "x86_64" => "x86_64",
        "aarch64" => "aarch_64",
        "i686" => "x86_32",
        "ppc64le" => "ppcle_64",
        "s390x" => "s390_64",
        _ => return None,
    })
}

pub fn ensure_protoc(cfg: &Config) -> Result<()> {
    if command_version_output("protoc", "--version").is_some() {
        tracing::info!("protoc present");
        return Ok(());
    }
    std::fs::create_dir_all(cfg.toolchain_dir().join("bin")).context("creating toolchain bin dir")?;
    std::fs::create_dir_all(cfg.src_dir()).context("creating scratch dir")?;

    let names = PkgNames {
        apt: "protobuf-compiler",
        dnf: "protobuf-compiler",
        pacman: "protobuf",
        apk: "protobuf",
        zypper: "protobuf-devel",
        xbps: "protobuf",
    };
    if pkg_install("protoc", &names)? && command_version_output("protoc", "--version").is_some() {
        tracing::info!("protoc installed via the system package manager");
        return Ok(());
    }

    let arch = cfg.arch();
    let Some(pb_arch) = protoc_arch(&arch) else {
        anyhow::bail!(
            "no official protoc release for arch '{arch}' and no package manager provided one -- \
             the from-source fallback (build_protoc_from_source in toolchain-protoc.sh) is not yet \
             ported here"
        );
    };
    const VERSION: &str = "25.3";
    let zip_name = format!("protoc-{VERSION}-linux-{pb_arch}.zip");
    let zip_path = cfg.src_dir().join(&zip_name);
    tracing::info!(arch = pb_arch, "fetching official protoc release");
    fetch_url(
        &format!("https://github.com/protocolbuffers/protobuf/releases/download/v{VERSION}/{zip_name}"),
        &zip_path,
    )
    .context("fetching protoc release")?;

    let dist_dir = cfg.toolchain_dir().join("protoc-dist");
    let status = std::process::Command::new("unzip")
        .args(["-oq"])
        .arg(&zip_path)
        .arg("-d")
        .arg(&dist_dir)
        .status()
        .context("running unzip on the protoc release")?;
    anyhow::ensure!(status.success(), "unzip failed extracting {}", zip_path.display());

    let target = cfg.toolchain_dir().join("bin/protoc");
    let _ = std::fs::remove_file(&target);
    #[cfg(unix)]
    std::os::unix::fs::symlink(dist_dir.join("bin/protoc"), &target)
        .context("symlinking protoc into the toolchain bin dir")?;
    tracing::info!(path = %target.display(), "protoc ready");
    Ok(())
}
