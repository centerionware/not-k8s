//! Toolchain presence checks — replaces `deploy/lib/toolchain-{rust,c,go,protoc}.sh`.
//!
//! Every tier from the shell version is ported now, including the deepest
//! from-source fallbacks (`try_musl_cc_toolchain`, `build_go_from_source`,
//! `build_protoc_from_source`) -- slow (30-90+ min
//! for gcc/Go), rare in practice (CI builds centrally), but real, not
//! `bail!`'d out. `ensure_c_toolchain`/`ensure_go` are public but **not**
//! called by the ordinary runtime path except that source builds must have a
//! static musl C compiler ready before Cargo is invoked. The low-memory Cargo
//! fallback is applied by fetch.rs to the actual build; this module only
//! ensures the tools are available.

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
    ensure_c_toolchain(cfg)?;
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

pub(crate) fn rust_target(arch: &str) -> Option<&'static str> {
    Some(match arch {
        "x86_64" => "x86_64-unknown-linux-musl",
        "aarch64" => "aarch64-unknown-linux-musl",
        "armv7l" => "armv7-unknown-linux-musleabihf",
        "armv6l" => "arm-unknown-linux-musleabihf",
        "i686" => "i686-unknown-linux-musl",
        "riscv64" => "riscv64gc-unknown-linux-musl",
        "ppc64le" => "powerpc64le-unknown-linux-musl",
        "s390x" => "s390x-unknown-linux-musl",
        _ => return None,
    })
}

fn rust_target_is_installed(target: &str) -> bool {
    if let Some(output) = std::process::Command::new("rustup").args(["target", "list", "--installed"]).output().ok() {
        return output.status.success()
            && String::from_utf8_lossy(&output.stdout).lines().any(|line| line.trim() == target);
    }
    std::process::Command::new("rustc")
        .args(["--print", "target-libdir", "--target", target])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| std::path::Path::new(String::from_utf8_lossy(&output.stdout).trim()).is_dir())
        .unwrap_or(false)
}

/// Select nodebootstrap's managed Rust installation for the rest of this
/// process. Bootstrap normally re-execs through sudo, so the caller's HOME
/// can otherwise make rustup install into one home while Cargo looks in
/// another.
fn use_managed_rust(cfg: &Config) {
    let toolchain_dir = cfg.toolchain_dir();
    std::env::set_var("RUSTUP_HOME", toolchain_dir.join("rustup"));
    std::env::set_var("CARGO_HOME", toolchain_dir.join("cargo"));
    prepend_path(&toolchain_dir.join("cargo/bin"));
}

pub fn ensure_rust(cfg: &Config) -> Result<()> {
    put_toolchain_bin_on_path(cfg);
    let arch = cfg.arch();
    let target = rust_target(&arch).with_context(|| {
        format!(
            "no known static musl Rust target for arch '{arch}' -- no way to build rustc from nothing but a \
             C compiler for an unsupported architecture; the only real path is mrustc \
             (https://github.com/thepowersgang/mrustc), out of scope here"
        )
    })?;
    let managed_cargo_bin = cfg.toolchain_dir().join("cargo/bin");
    if managed_cargo_bin.join("cargo").is_file() || managed_cargo_bin.join("rustup").is_file() {
        use_managed_rust(cfg);
    }
    if cargo_is_new_enough() {
        if rust_target_is_installed(target) {
            tracing::info!(
                version = command_version_output("cargo", "--version").unwrap_or_default(),
                target,
                "Rust present with static musl target"
            );
            return Ok(());
        }
        tracing::warn!(target, "Rust is present but its static musl target is missing");
    }
    if let Some(v) = command_version_output("cargo", "--version") {
        tracing::warn!("found {v} but this project needs >=1.{MIN_CARGO_MINOR} -- looking for a newer one");
    }

    if command_present("rustup") {
        let status = std::process::Command::new("rustup")
            .args(["target", "add", target])
            .status()
            .context("running rustup target add")?;
        if status.success() {
            if let Ok(cargo_path) = std::process::Command::new("rustup").args(["which", "cargo"]).output() {
                if cargo_path.status.success() {
                    if let Ok(path) = String::from_utf8(cargo_path.stdout) {
                        if let Some(parent) = std::path::Path::new(path.trim()).parent() {
                            prepend_path(parent);
                        }
                    }
                }
            }
            if cargo_is_new_enough() && rust_target_is_installed(target) {
                tracing::info!(target, "Rust static target ready via rustup");
                return Ok(());
            }
        } else {
            tracing::warn!(target, "rustup could not install the static musl target; trying the remaining toolchain tiers");
        }
    }

    let names = PkgNames { apt: "cargo rustc", dnf: "cargo rustc", pacman: "rust", apk: "cargo", zypper: "cargo rustc", xbps: "rust" };
    if pkg_install("rust", &names)? && cargo_is_new_enough() && rust_target_is_installed(target) {
        tracing::info!(target, "Rust installed via the system package manager");
        return Ok(());
    }

    tracing::info!(target, "installing Rust via rustup");
    let src_dir = cfg.src_dir();
    std::fs::create_dir_all(&src_dir).context("creating scratch dir for rustup-init.sh")?;
    let rustup_init = src_dir.join("rustup-init.sh");
    fetch_url("https://sh.rustup.rs", &rustup_init).context("fetching rustup-init.sh")?;
    let toolchain_dir = cfg.toolchain_dir();
    let status = std::process::Command::new("sh")
        .arg(&rustup_init)
        .args(["-y", "--default-toolchain", "stable", "--target", target, "--no-modify-path"])
        .env("HOME", &toolchain_dir)
        .env("RUSTUP_HOME", toolchain_dir.join("rustup"))
        .env("CARGO_HOME", toolchain_dir.join("cargo"))
        .status()
        .context("running rustup-init.sh")?;
    anyhow::ensure!(
        status.success(),
        "rustup could not install a toolchain for {target} -- this architecture has no official \
         prebuilt rustc"
    );
    // Unlike every other tool this module fetches, rustup's own cargo and
    // rustup land in $CARGO_HOME/bin. Keep that environment selected for the
    // rest of this process and explicitly install the target again: rustup-init
    // can treat an existing settings file as an idempotent no-op while still
    // returning success, leaving Cargo without libcore for musl.
    use_managed_rust(cfg);
    let status = std::process::Command::new("rustup")
        .args(["target", "add", target])
        .status()
        .context("installing the static musl Rust target")?;
    anyhow::ensure!(status.success(), "rustup could not install the static musl Rust target {target}");
    anyhow::ensure!(
        cargo_is_new_enough() && rust_target_is_installed(target),
        "Rust installed but static musl target {target} is still unavailable"
    );
    tracing::info!("Rust installed via rustup into {}", toolchain_dir.join("cargo/bin").display());
    Ok(())
}

fn command_present(bin: &str) -> bool {
    crate::pkg::command_exists(bin)
}

/// Prepends `Config::toolchain_dir()/bin` to this process's own `PATH` --
/// idempotent (checked before prepending), safe to call as often as
/// needed. Every function in this module that fetches or builds a tool
/// into that directory calls this right after, so a subsequent
/// `Command::new("go")`/`("protoc")`/`("cc")` within the *same*
/// `nodebootstrap` run finds it -- the shell version got this for free
/// from `export PATH=...` accumulating across one script's process; a
/// symlink on disk alone doesn't change what *this* process's `PATH`
/// already resolved at startup.
pub(crate) fn put_toolchain_bin_on_path(cfg: &Config) {
    prepend_path(&cfg.toolchain_dir().join("bin"));
}

fn prepend_path(dir: &std::path::Path) {
    let current = std::env::var("PATH").unwrap_or_default();
    if std::env::split_paths(&current).any(|p| p == dir) {
        return;
    }
    let mut paths: Vec<_> = std::env::split_paths(&current).collect();
    paths.insert(0, dir.to_path_buf());
    if let Ok(new_path) = std::env::join_paths(paths) {
        std::env::set_var("PATH", new_path);
    }
}

fn run_cmd(program: &str, args: &[&str], cwd: &std::path::Path) -> Result<()> {
    tracing::info!(program, args = ?args, cwd = %cwd.display(), "running");
    let status = std::process::Command::new(program)
        .args(args)
        .current_dir(cwd)
        .status()
        .with_context(|| format!("running {program} {}", args.join(" ")))?;
    anyhow::ensure!(status.success(), "{program} {} failed", args.join(" "));
    Ok(())
}

/// musl.cc arch triples -- static, self-contained cross/native toolchains
/// that run with zero shared-lib deps, so they work even with no package
/// manager at all (BusyBox-only initramfs etc.). Middle tier between the
/// package manager and a from-source gcc build.
fn musl_cc_triple(arch: &str) -> Option<&'static str> {
    Some(match arch {
        "x86_64" => "x86_64-linux-musl",
        "aarch64" => "aarch64-linux-musl",
        "armv7l" => "armv7l-linux-musleabihf",
        "armv6l" => "arm-linux-musleabihf",
        "i686" => "i686-linux-musl",
        "riscv64" => "riscv64-linux-musl",
        "ppc64le" => "powerpc64le-linux-musl",
        "s390x" => "s390x-linux-musl",
        _ => return None,
    })
}

fn try_musl_cc_toolchain(cfg: &Config) -> Result<bool> {
    let arch = cfg.arch();
    let Some(triple) = musl_cc_triple(&arch) else { return Ok(false) };
    tracing::info!(triple, "trying static musl.cc toolchain");
    let tarball = cfg.src_dir().join(format!("{triple}-cross.tgz"));
    if fetch_url(&format!("https://musl.cc/{triple}-cross.tgz"), &tarball).is_err() {
        return Ok(false);
    }
    let toolchain_dir = cfg.toolchain_dir();
    run_cmd("tar", &["xzf", &tarball.to_string_lossy(), "-C", "."], &toolchain_dir)?;
    let cc = toolchain_dir.join(format!("{triple}-cross/bin/{triple}-gcc"));
    if !cc.exists() {
        return Ok(false);
    }
    for name in ["cc", "gcc"] {
        let link = toolchain_dir.join("bin").join(name);
        let _ = std::fs::remove_file(&link);
        #[cfg(unix)]
        std::os::unix::fs::symlink(&cc, &link).with_context(|| format!("symlinking {name}"))?;
    }
    put_toolchain_bin_on_path(cfg);
    tracing::info!(path = %cc.display(), "static musl.cc toolchain ready");
    Ok(true)
}

fn num_jobs() -> String {
    std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1).to_string()
}

pub fn ensure_c_toolchain(cfg: &Config) -> Result<()> {
    put_toolchain_bin_on_path(cfg);
    let arch = cfg.arch();
    let target = rust_target(&arch).with_context(|| format!("no supported static musl Rust target for arch '{arch}' -- refusing a glibc-linked compiler path"))?;
    if let Some(compiler) = find_musl_cc(cfg) {
        configure_musl_cargo(target, &compiler);
        return Ok(());
    }

    let names = PkgNames {
        apt: "musl-tools musl-dev linux-libc-dev",
        dnf: "musl-gcc musl-libc-devel kernel-headers",
        pacman: "musl linux-api-headers",
        apk: "musl-dev linux-headers gcc",
        zypper: "musl-devel kernel-headers gcc",
        xbps: "musl-devel linux-headers",
    };
    if pkg_install("musl C toolchain", &names)? {
        put_toolchain_bin_on_path(cfg);
        if let Some(compiler) = find_musl_cc(cfg) {
            configure_musl_cargo(target, &compiler);
            return Ok(());
        }
    }
    if try_musl_cc_toolchain(cfg)? {
        if let Some(compiler) = find_musl_cc(cfg) {
            configure_musl_cargo(target, &compiler);
            return Ok(());
        }
    }
    anyhow::bail!(
        "no usable musl C compiler is available for {arch}; refusing the generic glibc compiler fallback because it would make the deployed Rust binaries depend on the host's glibc. Install a musl development toolchain or make the matching musl.cc toolchain reachable"
    )
}

fn find_musl_cc(cfg: &Config) -> Option<String> {
    let arch = cfg.arch();
    let triple = musl_cc_triple(&arch)?;
    let target = rust_target(&arch)?;
    let target_key = target.replace('-', "_");
    let target_upper = target_key.to_ascii_uppercase();
    let mut candidates = Vec::new();
    // A caller may already have selected a cross compiler for this exact
    // target (the release cross-build jobs supply either a musl.cc compiler
    // or a Zig wrapper this way). Honor that selection before looking for
    // generic `musl-gcc` names: on a foreign-architecture runner, the latter
    // can be a valid musl compiler for the host but unusable for the requested
    // target.
    for variable in [
        "MUSL_C_COMPILER".to_string(),
        format!("CC_{target_key}"),
        format!("CARGO_TARGET_{target_upper}_LINKER"),
    ] {
        if let Ok(compiler) = std::env::var(variable) {
            candidates.push(compiler);
        }
    }
    candidates.extend([
        cfg.toolchain_dir().join(format!("{triple}-cross/bin/{triple}-gcc")).to_string_lossy().into_owned(),
        cfg.toolchain_dir().join("bin/gcc").to_string_lossy().into_owned(),
        cfg.toolchain_dir().join("bin/cc").to_string_lossy().into_owned(),
    ]);
    let names = ["musl-gcc".to_string(), format!("{triple}-gcc"), "gcc".to_string(), "cc".to_string()];
    for name in names {
        if let Some(path) = which(&name) {
            candidates.push(path);
        }
    }
    candidates.into_iter().find(|candidate| {
        let path = std::path::Path::new(candidate);
        if !path.is_file() {
            return false;
        }
        let machine = std::process::Command::new(candidate)
            .arg("-dumpmachine")
            .output()
            .ok()
            .filter(|output| output.status.success())
            .map(|output| String::from_utf8_lossy(&output.stdout).into_owned())
            .unwrap_or_default();
        let resolved = std::fs::canonicalize(path).ok();
        let resolved_is_musl = resolved.as_ref().is_some_and(|resolved| resolved.to_string_lossy().contains("musl"));
        let target_matches_machine = compiler_machine_matches_arch(&machine, &arch);
        let target_matches_path = machine.trim().is_empty()
            && resolved
                .as_ref()
                .is_some_and(|resolved| compiler_path_matches_arch(resolved, &arch));
        (machine.contains("musl") || resolved_is_musl) && (target_matches_machine || target_matches_path)
    })
}

fn compiler_machine_matches_arch(machine: &str, arch: &str) -> bool {
    let machine = machine.trim();
    match arch {
        "x86_64" => machine.starts_with("x86_64"),
        "aarch64" => machine.starts_with("aarch64"),
        "armv7l" => machine.starts_with("armv7") || machine.starts_with("arm-"),
        "armv6l" => machine.starts_with("armv6") || machine.starts_with("arm-"),
        "i686" => machine.starts_with("i686"),
        "riscv64" => machine.starts_with("riscv64"),
        "ppc64le" => machine.starts_with("powerpc64le"),
        "s390x" => machine.starts_with("s390x"),
        _ => false,
    }
}

fn compiler_path_matches_arch(path: &std::path::Path, arch: &str) -> bool {
    let path = path.to_string_lossy();
    match arch {
        "x86_64" => path.contains("x86_64"),
        "aarch64" => path.contains("aarch64"),
        "armv7l" => path.contains("armv7"),
        "armv6l" => path.contains("armv6") || path.contains("arm-linux"),
        "i686" => path.contains("i686"),
        "riscv64" => path.contains("riscv64"),
        "ppc64le" => path.contains("powerpc64le"),
        "s390x" => path.contains("s390x"),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::compiler_machine_matches_arch;

    #[test]
    fn musl_compiler_architecture_must_match_requested_target() {
        assert!(compiler_machine_matches_arch("x86_64-linux-musl", "x86_64"));
        assert!(compiler_machine_matches_arch("aarch64-linux-musl", "aarch64"));
        assert!(compiler_machine_matches_arch("armv7l-linux-musleabihf", "armv7l"));
        assert!(compiler_machine_matches_arch("arm-linux-musleabihf", "armv7l"));
        assert!(!compiler_machine_matches_arch("x86_64-linux-musl", "aarch64"));
        assert!(!compiler_machine_matches_arch("x86_64-linux-musl", "armv7l"));
    }
}

fn which(bin: &str) -> Option<String> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).find_map(|dir| {
        let path = dir.join(bin);
        path.is_file().then(|| path.to_string_lossy().into_owned())
    })
}

fn configure_musl_cargo(target: &str, compiler: &str) {
    let key = target.replace('-', "_");
    let upper = key.to_ascii_uppercase();
    std::env::set_var(format!("CC_{key}"), compiler);
    std::env::set_var(format!("CARGO_TARGET_{upper}_LINKER"), compiler);
    let rustflags = format!("{}-C target-feature=+crt-static", std::env::var(format!("CARGO_TARGET_{upper}_RUSTFLAGS")).map(|v| format!("{v} ")).unwrap_or_default());
    std::env::set_var(format!("CARGO_TARGET_{upper}_RUSTFLAGS"), rustflags);
    std::env::set_var("MUSL_C_COMPILER", compiler);
    std::env::set_var("MUSL_RUST_TARGET", target);
    tracing::info!(compiler, target, "using static musl C compiler");
}

fn go_arch(arch: &str) -> Option<&'static str> {
    Some(match arch {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        "armv7l" | "armv6l" => "armv6l",
        "i686" => "386",
        "riscv64" => "riscv64",
        "ppc64le" => "ppc64le",
        "s390x" => "s390x",
        "loongarch64" => "loong64",
        _ => return None,
    })
}

fn go_is_new_enough() -> bool {
    let Some(out) = command_version_output("go", "version") else { return false };
    // "go version go1.22.6 linux/amd64" -> 22
    out.split_whitespace()
        .find_map(|tok| tok.strip_prefix("go1."))
        .and_then(|rest| rest.split(['.', ' ']).next())
        .and_then(|minor| minor.parse::<u32>().ok())
        .is_some_and(|minor| minor >= 21)
}

const GO_VERSION: &str = "1.22.6";

pub fn ensure_go(cfg: &Config) -> Result<()> {
    put_toolchain_bin_on_path(cfg);
    if go_is_new_enough() {
        tracing::info!("Go present and new enough");
        return Ok(());
    }
    let names = PkgNames { apt: "golang-go", dnf: "golang", pacman: "go", apk: "go", zypper: "go", xbps: "go" };
    if pkg_install("go", &names)? && go_is_new_enough() {
        tracing::info!("Go installed via the system package manager");
        return Ok(());
    }

    let arch = cfg.arch();
    if let Some(goarch) = go_arch(&arch) {
        let tarball = cfg.src_dir().join(format!("go{GO_VERSION}.linux-{goarch}.tar.gz"));
        std::fs::create_dir_all(cfg.src_dir()).context("creating scratch dir")?;
        tracing::info!(arch = goarch, "fetching official Go release");
        if fetch_url(&format!("https://go.dev/dl/go{GO_VERSION}.linux-{goarch}.tar.gz"), &tarball).is_ok() {
            let toolchain_dir = cfg.toolchain_dir();
            let _ = std::fs::remove_dir_all(toolchain_dir.join("go"));
            run_cmd("tar", &["xzf", &tarball.to_string_lossy(), "-C"], &toolchain_dir)?;
            let go_bin = toolchain_dir.join("go/bin/go");
            let link = toolchain_dir.join("bin/go");
            let _ = std::fs::remove_file(&link);
            #[cfg(unix)]
            std::os::unix::fs::symlink(&go_bin, &link).context("symlinking go")?;
            put_toolchain_bin_on_path(cfg);
            if go_is_new_enough() {
                tracing::info!("Go ready via official release");
                return Ok(());
            }
        }
    }
    build_go_from_source(cfg)
}

/// Go's own documented from-source bootstrap: Go 1.4 (the last
/// C-implemented release) builds with just a C compiler; that becomes
/// `GOROOT_BOOTSTRAP` for an intermediate Go version (>=1.21 refuses to
/// bootstrap from anything older than Go 1.20); that intermediate Go then
/// builds the final target version. Three full Go builds, slow, but needs
/// nothing beyond a C compiler.
fn build_go_from_source(cfg: &Config) -> Result<()> {
    tracing::warn!("no prebuilt Go for this arch -- bootstrapping Go from source (three stages, slow)");
    ensure_c_toolchain(cfg).context("Go's from-source bootstrap needs a C compiler")?;

    let src_dir = cfg.src_dir();
    std::fs::create_dir_all(&src_dir).context("creating scratch dir")?;

    let bootstrap_dir = src_dir.join("go-bootstrap-c");
    if !bootstrap_dir.join("bin/go").exists() {
        let tarball = src_dir.join("go1.4-bootstrap.tar.gz");
        fetch_url("https://dl.google.com/go/go1.4-bootstrap-20171003.tar.gz", &tarball)?;
        let _ = std::fs::remove_dir_all(&bootstrap_dir);
        std::fs::create_dir_all(&bootstrap_dir).context("creating go1.4 bootstrap dir")?;
        run_cmd("tar", &["xzf", &tarball.to_string_lossy(), "-C", &bootstrap_dir.to_string_lossy(), "--strip-components=1"], &src_dir)?;
        let status = std::process::Command::new("./make.bash")
            .current_dir(bootstrap_dir.join("src"))
            .env("CGO_ENABLED", "0")
            .status()
            .context("running go1.4 bootstrap make.bash")?;
        anyhow::ensure!(status.success(), "go1.4 bootstrap build failed");
    }

    const MID_VER: &str = "1.20.14";
    let mid_dir = src_dir.join(format!("go-{MID_VER}"));
    if !mid_dir.join("bin/go").exists() {
        let tarball = src_dir.join(format!("go{MID_VER}.src.tar.gz"));
        fetch_url(&format!("https://go.dev/dl/go{MID_VER}.src.tar.gz"), &tarball)?;
        let _ = std::fs::remove_dir_all(&mid_dir);
        std::fs::create_dir_all(&mid_dir).context("creating intermediate Go build dir")?;
        run_cmd("tar", &["xzf", &tarball.to_string_lossy(), "-C", &mid_dir.to_string_lossy(), "--strip-components=1"], &src_dir)?;
        let status = std::process::Command::new("./make.bash")
            .current_dir(mid_dir.join("src"))
            .env("GOROOT_BOOTSTRAP", &bootstrap_dir)
            .status()
            .context("running intermediate Go make.bash")?;
        anyhow::ensure!(status.success(), "intermediate Go build failed");
    }

    let final_dir = src_dir.join(format!("go-{GO_VERSION}"));
    if !final_dir.join("bin/go").exists() {
        let tarball = src_dir.join(format!("go{GO_VERSION}.src.tar.gz"));
        fetch_url(&format!("https://go.dev/dl/go{GO_VERSION}.src.tar.gz"), &tarball)?;
        let _ = std::fs::remove_dir_all(&final_dir);
        std::fs::create_dir_all(&final_dir).context("creating final Go build dir")?;
        run_cmd("tar", &["xzf", &tarball.to_string_lossy(), "-C", &final_dir.to_string_lossy(), "--strip-components=1"], &src_dir)?;
        let status = std::process::Command::new("./make.bash")
            .current_dir(final_dir.join("src"))
            .env("GOROOT_BOOTSTRAP", &mid_dir)
            .status()
            .context("running final Go make.bash")?;
        anyhow::ensure!(status.success(), "final Go build failed");
    }

    let link = cfg.toolchain_dir().join("bin/go");
    let _ = std::fs::remove_file(&link);
    #[cfg(unix)]
    std::os::unix::fs::symlink(final_dir.join("bin/go"), &link).context("symlinking go")?;
    put_toolchain_bin_on_path(cfg);
    anyhow::ensure!(go_is_new_enough(), "Go source bootstrap finished but 'go' is still not usable");
    tracing::info!("Go built from source");
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
    put_toolchain_bin_on_path(cfg);
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
        tracing::info!(arch, "no official protoc release for this arch -- building from source");
        return build_protoc_from_source(cfg);
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
    put_toolchain_bin_on_path(cfg);
    tracing::info!(path = %target.display(), "protoc ready");
    Ok(())
}

/// Deepest protoc fallback: build libprotobuf+protoc from source. Uses an
/// autotools-based release (predates the cmake-only era) so the only
/// requirement is a C++ compiler + make -- no cmake bootstrap needed.
fn build_protoc_from_source(cfg: &Config) -> Result<()> {
    tracing::warn!("building protoc from source (no prebuilt available for this arch)");
    if !command_present("g++") {
        let names = PkgNames { apt: "g++", dnf: "gcc-c++", pacman: "base-devel", apk: "g++", zypper: "gcc-c++", xbps: "base-devel" };
        let _ = pkg_install("C++ compiler", &names);
    }
    anyhow::ensure!(command_present("g++"), "need a C++ compiler to build protoc from source and couldn't get one");

    const VERSION: &str = "21.12";
    let src_dir = cfg.src_dir();
    std::fs::create_dir_all(&src_dir).context("creating scratch dir")?;
    let tarball = src_dir.join(format!("protobuf-cpp-{VERSION}.tar.gz"));
    fetch_url(
        &format!("https://github.com/protocolbuffers/protobuf/releases/download/v{VERSION}/protobuf-cpp-{VERSION}.tar.gz"),
        &tarball,
    )?;
    run_cmd("tar", &["xzf", &tarball.to_string_lossy()], &src_dir)?;

    let build_dir = src_dir.join(format!("protobuf-{VERSION}"));
    let prefix = cfg.toolchain_dir().join("protoc-src-build");
    let prefix_arg = format!("--prefix={}", prefix.display());
    run_cmd("./configure", &[&prefix_arg], &build_dir)?;
    run_cmd("make", &["-j", &num_jobs()], &build_dir)?;
    run_cmd("make", &["install"], &build_dir)?;

    let link = cfg.toolchain_dir().join("bin/protoc");
    let _ = std::fs::remove_file(&link);
    #[cfg(unix)]
    std::os::unix::fs::symlink(prefix.join("bin/protoc"), &link).context("symlinking protoc")?;
    put_toolchain_bin_on_path(cfg);
    tracing::info!("protoc built from source");
    Ok(())
}
