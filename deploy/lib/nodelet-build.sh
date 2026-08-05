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
build_nodelet() {
    if [[ -n "${NOTK8S_NODELET_PREBUILT:-}" ]]; then
        log "Using prebuilt nodelet binary: $NOTK8S_NODELET_PREBUILT"
        [[ -x "$NOTK8S_NODELET_PREBUILT" ]] || die "NOTK8S_NODELET_PREBUILT is set but not an executable file: $NOTK8S_NODELET_PREBUILT"
        mkdir -p "$REPO_ROOT/bin"
        install -m 0755 "$NOTK8S_NODELET_PREBUILT" "$REPO_ROOT/bin/nodelet"
        return 0
    fi

    cd "$REPO_ROOT"
    local features=()
    if [[ "$WITH_CRI" -eq 1 ]]; then
        ensure_protoc
        features=(--features cri)
    fi
    log "Building nodelet (cargo build --release ${features[*]:-})..."
    if ! cargo build --release "${features[@]}"; then
        # Cargo.toml's [profile.release] deliberately uses lto = true +
        # codegen-units = 1 for the smallest/fastest possible edge binary —
        # right for a well-resourced build machine/CI, but it means nearly
        # all of the actual compiling (every dependency crate: tokio, kube,
        # and with --features cri also tonic/prost/rustls) happens fine,
        # and then the *entire* dependency graph gets merged into one
        # codegen unit and LTO'd in a single rustc/LLVM process at the very
        # end. That one process's memory use scales with the whole
        # dependency graph, not any single crate — on a constrained device
        # it's the single likeliest place in this whole script for the OOM
        # killer to strike, and from the outside that looks exactly like
        # "the build got all the way to the end and then just died/hung."
        # Confirmed for real: this is the actual failure being retried here.
        #
        # Don't touch the committed profile for that — retry once with a
        # much lighter LTO/codegen setting via env override instead, same
        # tiered-fallback spirit as everything else in this script.
        warn "cargo build --release failed — if this device is memory-constrained, the likely cause is the final whole-program LTO step (Cargo.toml's [profile.release] uses lto=true, codegen-units=1 for the smallest edge binary, which needs the most memory right at the end). Retrying once with lighter LTO settings (thin LTO, 16 codegen units) that trade a slightly larger binary for much lower peak memory..."
        CARGO_PROFILE_RELEASE_LTO=thin CARGO_PROFILE_RELEASE_CODEGEN_UNITS=16 \
            cargo build --release "${features[@]}" \
            || die "Build failed even with lighter LTO settings — check $LOG_DIR or run 'free -h' during a manual 'cargo build --release' to confirm this is memory exhaustion (dmesg/journalctl will show an oom-kill of rustc/cc1plus/ld if so). Adding swap, or building on a box with more RAM, are the remaining options for this profile."
    fi
    [[ -x "$REPO_ROOT/target/release/nodelet" ]] || die "Build finished but binary not found."

    # Copy out to a stable path before the end-of-run cleanup wipes target/
    # (the whole cargo build cache — deps, incremental, fingerprints — none
    # of which is needed once the binary exists).
    mkdir -p "$REPO_ROOT/bin"
    install -m 0755 "$REPO_ROOT/target/release/nodelet" "$REPO_ROOT/bin/nodelet"
    log "nodelet built: $REPO_ROOT/bin/nodelet"
}
