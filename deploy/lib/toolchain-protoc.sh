# lib/toolchain-protoc.sh — protoc, only needed for --with-cri (the mock
# runtime needs no gRPC/protobuf code generation).

ensure_protoc() {
    [[ "$WITH_CRI" -eq 1 ]] || return 0
    if command -v protoc &>/dev/null; then
        log "protoc present: $(protoc --version)"
        return 0
    fi

    # bootstrap-source.sh creates these, but the CI composite setup action
    # deliberately only creates its log directory. The package path normally
    # hides that difference; an apt timeout exposes it when either fallback
    # needs to write under SRC_DIR or TOOLCHAIN_DIR.
    mkdir -p "$WORK_DIR" "$TOOLCHAIN_DIR" "$TOOLCHAIN_DIR/bin" "$SRC_DIR" "$LOG_DIR"

    pkg_install "protoc" "protobuf-compiler" "protobuf-compiler" "protobuf" "protobuf" "protobuf-devel" "protobuf" \
        && command -v protoc &>/dev/null && return 0

    local pb_arch=""
    case "$ARCH" in
        x86_64)  pb_arch=x86_64 ;;
        aarch64) pb_arch=aarch_64 ;;
        i686)    pb_arch=x86_32 ;;
        ppc64le) pb_arch=ppcle_64 ;;
        s390x)   pb_arch=s390_64 ;;
        *)       pb_arch="" ;;
    esac

    if [[ -n "$pb_arch" ]]; then
        log "Fetching official protoc release for linux-$pb_arch..."
        local ver=25.3
        local zip="protoc-$ver-linux-$pb_arch.zip"
        if fetch "https://github.com/protocolbuffers/protobuf/releases/download/v$ver/$zip" "$SRC_DIR/$zip"; then
            ( cd "$TOOLCHAIN_DIR" && unzip -oq "$SRC_DIR/$zip" -d protoc-dist )
            ln -sf "$TOOLCHAIN_DIR/protoc-dist/bin/protoc" "$TOOLCHAIN_DIR/bin/protoc"
            command -v protoc &>/dev/null && { log "protoc ready: $(protoc --version)"; return 0; }
        fi
    fi

    build_protoc_from_source
}

# Deepest protoc fallback: build libprotobuf+protoc from source. Uses an
# autotools-based release (predates the cmake-only era) so the only
# requirement is a C++ compiler + make — no cmake bootstrap needed.
build_protoc_from_source() {
    log "Building protoc from source (no prebuilt available for $ARCH)..."
    command -v g++ &>/dev/null || pkg_install "C++ compiler" "g++" "gcc-c++" "base-devel" "g++" "gcc-c++" "base-devel" || true
    command -v g++ &>/dev/null || die "Need a C++ compiler to build protoc from source and couldn't get one."

    local ver=21.12
    cd "$SRC_DIR"
    local tarball="protobuf-cpp-$ver.tar.gz"
    fetch "https://github.com/protocolbuffers/protobuf/releases/download/v$ver/$tarball" "$tarball"
    tar xzf "$tarball"
    ( cd "protobuf-$ver" && ./configure --prefix="$TOOLCHAIN_DIR/protoc-src-build" \
        && make -j"$(nproc)" && make install )
    ln -sf "$TOOLCHAIN_DIR/protoc-src-build/bin/protoc" "$TOOLCHAIN_DIR/bin/protoc"
    log "protoc built from source: $(protoc --version)"
}
