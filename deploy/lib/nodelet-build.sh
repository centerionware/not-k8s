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
    cargo build --release "${features[@]}"
    [[ -x "$REPO_ROOT/target/release/nodelet" ]] || die "Build finished but binary not found."

    # Copy out to a stable path before the end-of-run cleanup wipes target/
    # (the whole cargo build cache — deps, incremental, fingerprints — none
    # of which is needed once the binary exists).
    mkdir -p "$REPO_ROOT/bin"
    install -m 0755 "$REPO_ROOT/target/release/nodelet" "$REPO_ROOT/bin/nodelet"
    log "nodelet built: $REPO_ROOT/bin/nodelet"
}
