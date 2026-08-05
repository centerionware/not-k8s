# lib/nodelet-build.sh — produce $REPO_ROOT/bin/nodelet.
#
# Today this always builds from source on-device (rustc + optionally protoc,
# via toolchain-rust.sh / toolchain-protoc.sh). The eventual plan is a
# `bootstrap-binary-latest.sh` entry point that fetches a prebuilt nodelet
# from a GitHub Actions release for this device's arch/libc instead of
# compiling locally — this function is the seam for that: it already checks
# for a prebuilt drop-in before building, so that future script only has to
# populate $NOTK8S_NODELET_PREBUILT (or $REPO_ROOT/bin/nodelet directly)
# before sourcing this file, and build_nodelet() becomes a no-op. Nothing
# about that split needs designing later — it's just not wired up to CI yet.

# install_nodelet_binary <source-path> — copies a built/prebuilt binary to
# $REPO_ROOT/bin/nodelet, overwriting whatever's already there (a stale
# binary from a previous run, possibly owned by a different user if that
# run's privileges differed from this one's). Split out from build_nodelet()
# because a plain `install -m 0755 src dst` silently no-ops on a permission
# error against an existing dst on some systems' coreutils rather than
# failing loudly — confirmed for real: an earlier root-owned bin/nodelet
# left an unprivileged rebuild's `install` failing with "Permission denied"
# while the surrounding script logged success anyway and moved on, leaving
# the OLD binary running with nothing about the output saying so. rm -f
# first so the second install has nothing to fail to overwrite; if THAT
# still fails (e.g. bin/ itself isn't writable by this user), die instead of
# silently leaving a stale binary in place.
install_nodelet_binary() {
    local src="$1"
    mkdir -p "$REPO_ROOT/bin"
    rm -f "$REPO_ROOT/bin/nodelet" 2>/dev/null
    install -m 0755 "$src" "$REPO_ROOT/bin/nodelet" \
        || die "Couldn't install $src to $REPO_ROOT/bin/nodelet (permission denied? check ownership of $REPO_ROOT/bin — a previous run under different privileges, e.g. sudo vs. not, can leave it owned by another user). The build itself succeeded; only this final copy step failed, so re-running with correct permissions on $REPO_ROOT/bin should be all that's needed."
    [[ -x "$REPO_ROOT/bin/nodelet" ]] || die "install reported success but $REPO_ROOT/bin/nodelet still isn't there/executable — filesystem full? check df -h."
}

# release_lto_settings_for_this_device — echoes env-var assignments
# (CARGO_PROFILE_RELEASE_LTO=... CARGO_PROFILE_RELEASE_CODEGEN_UNITS=...) to
# eval before cargo build. Cargo.toml's committed [profile.release] uses
# lto=true, codegen-units=1 for the smallest/fastest edge binary — right for
# a well-resourced build machine/CI, but it means nearly all of the actual
# compiling (every dependency crate: tokio, kube, and with --features cri
# also tonic/prost/rustls) happens fine, and then the *entire* dependency
# graph gets merged into one codegen unit and LTO'd in a single rustc/LLVM
# process at the very end. That one process's memory use scales with the
# whole dependency graph, not any single crate.
#
# On a well-resourced machine that's just slow. On a genuinely
# memory-constrained device (confirmed for real on a ~2.8GB-RAM box) it can
# OOM-kill hard enough to take the whole host down with it, not just the
# rustc process — which means the ordinary "build fails, retry with lighter
# settings" fallback below never gets a chance to run at all, and whoever's
# running this script sees the box reboot mid-build with no diagnostic,
# left with whatever stale bin/nodelet happened to already be there
# (install_nodelet_binary above at least makes that visible on the *next*
# successful run, but the crashed run itself gives no signal). Cheaper to
# just not attempt the risky profile at all below a conservative memory
# floor than to rely on a retry that a hard crash preempts.
release_lto_settings_for_this_device() {
    local total_kb
    total_kb="$(awk '/^MemTotal:/{print $2}' /proc/meminfo 2>/dev/null || echo 0)"
    # ~4GB floor: comfortably above the ~2.8GB device this was confirmed
    # on (where even the *lighter* thin-LTO/16-codegen-unit build peaked
    # under 1GB free), comfortably below any real build server/CI runner.
    if [[ "$total_kb" -gt 0 && "$total_kb" -lt 4194304 ]]; then
        echo "CARGO_PROFILE_RELEASE_LTO=thin CARGO_PROFILE_RELEASE_CODEGEN_UNITS=16"
    fi
}

build_nodelet() {
    if [[ -n "${NOTK8S_NODELET_PREBUILT:-}" ]]; then
        log "Using prebuilt nodelet binary: $NOTK8S_NODELET_PREBUILT"
        [[ -x "$NOTK8S_NODELET_PREBUILT" ]] || die "NOTK8S_NODELET_PREBUILT is set but not an executable file: $NOTK8S_NODELET_PREBUILT"
        install_nodelet_binary "$NOTK8S_NODELET_PREBUILT"
        return 0
    fi

    cd "$REPO_ROOT"
    local features=()
    if [[ "$WITH_CRI" -eq 1 ]]; then
        ensure_protoc
        features=(--features cri)
    fi

    local lto_override
    lto_override="$(release_lto_settings_for_this_device)"
    if [[ -n "$lto_override" ]]; then
        log "This device has under 4GB RAM — building with lighter LTO settings ($lto_override) from the start instead of risking the full lto=true/codegen-units=1 profile's memory spike."
    fi
    log "Building nodelet (cargo build --release ${features[*]:-})..."
    if ! env $lto_override cargo build --release "${features[@]}"; then
        if [[ -n "$lto_override" ]]; then
            die "Build failed even with lighter LTO settings — check $LOG_DIR or run 'free -h' during a manual 'cargo build --release' to confirm this is memory exhaustion (dmesg/journalctl will show an oom-kill of rustc/cc1plus/ld if so). Adding swap, or building on a box with more RAM, are the remaining options for this profile."
        fi
        # Confirmed for real: this is the actual failure being retried
        # here — see release_lto_settings_for_this_device()'s comment for
        # why a big-enough device still tries the expensive profile first.
        warn "cargo build --release failed — if this device is memory-constrained, the likely cause is the final whole-program LTO step (Cargo.toml's [profile.release] uses lto=true, codegen-units=1 for the smallest edge binary, which needs the most memory right at the end). Retrying once with lighter LTO settings (thin LTO, 16 codegen units) that trade a slightly larger binary for much lower peak memory..."
        CARGO_PROFILE_RELEASE_LTO=thin CARGO_PROFILE_RELEASE_CODEGEN_UNITS=16 \
            cargo build --release "${features[@]}" \
            || die "Build failed even with lighter LTO settings — check $LOG_DIR or run 'free -h' during a manual 'cargo build --release' to confirm this is memory exhaustion (dmesg/journalctl will show an oom-kill of rustc/cc1plus/ld if so). Adding swap, or building on a box with more RAM, are the remaining options for this profile."
    fi
    [[ -x "$REPO_ROOT/target/release/nodelet" ]] || die "Build finished but binary not found."

    # Copy out to a stable path before the end-of-run cleanup wipes target/
    # (the whole cargo build cache — deps, incremental, fingerprints — none
    # of which is needed once the binary exists).
    install_nodelet_binary "$REPO_ROOT/target/release/nodelet"
    log "nodelet built: $REPO_ROOT/bin/nodelet"
}
