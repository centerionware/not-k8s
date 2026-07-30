# lib/toolchain-c.sh — C compiler tiering: package manager -> musl.cc static
# prebuilt -> build gcc from pure source. See bootstrap-source.sh's header for
# the overall dependency-tier strategy this and the other toolchain-*.sh
# files all follow.

# This is the deepest fallback for a C compiler: no prebuilt binaries at
# all, just GNU source tarballs + `make`. Slow (expect 30-60+ min on a
# modest embedded core) but has no binary dependency beyond a *minimal*
# working `cc` — which on some rare systems means: none at all is fine,
# because binutils+gcc's own build only needs a C library and a linker/
# assembler that ship with the kernel's toolchain seed... in practice this
# still needs *some* pre-existing `cc` (e.g. tcc, or the box already having
# a partial toolchain). True zero-binary bootstrap (seeding a compiler from
# nothing but a hex monitor) is out of scope for a test-deployment script.
build_gcc_from_source() {
    command -v cc &>/dev/null && { log "A cc is already present; skipping gcc source build."; return 0; }
    log "No C compiler and no package manager/prebuilt worked — building gcc from source."
    warn "This takes a long time (30-90+ minutes) on constrained hardware."

    local GCC_VER=13.2.0
    local BINUTILS_VER=2.42
    local PREFIX="$TOOLCHAIN_DIR/gcc-src-build"
    mkdir -p "$PREFIX"

    cd "$SRC_DIR"
    [[ -f "binutils-$BINUTILS_VER.tar.xz" ]] || fetch "https://ftp.gnu.org/gnu/binutils/binutils-$BINUTILS_VER.tar.xz" "binutils-$BINUTILS_VER.tar.xz"
    [[ -f "gcc-$GCC_VER.tar.xz" ]] || fetch "https://ftp.gnu.org/gnu/gcc/gcc-$GCC_VER/gcc-$GCC_VER.tar.xz" "gcc-$GCC_VER.tar.xz"
    tar xf "binutils-$BINUTILS_VER.tar.xz"
    tar xf "gcc-$GCC_VER.tar.xz"

    ( cd "gcc-$GCC_VER" && ./contrib/download_prerequisites )

    mkdir -p build-binutils && ( cd build-binutils && \
        "../binutils-$BINUTILS_VER/configure" --prefix="$PREFIX" --disable-nls --disable-werror && \
        make -j"$(nproc)" && make install )

    export PATH="$PREFIX/bin:$PATH"
    mkdir -p build-gcc && ( cd build-gcc && \
        "../gcc-$GCC_VER/configure" --prefix="$PREFIX" --disable-nls --disable-multilib \
            --enable-languages=c,c++ --disable-bootstrap && \
        make -j"$(nproc)" && make install )

    ln -sf "$PREFIX/bin/gcc" "$TOOLCHAIN_DIR/bin/cc"
    ln -sf "$PREFIX/bin/gcc" "$TOOLCHAIN_DIR/bin/gcc"
    ln -sf "$PREFIX/bin/g++" "$TOOLCHAIN_DIR/bin/g++"
    log "gcc built from source: $("$TOOLCHAIN_DIR/bin/gcc" --version | head -1)"
}

# musl.cc publishes static, self-contained cross/native toolchains for a
# wide arch matrix. They run on the target arch with zero shared-lib deps,
# so they work even on distros with no package manager at all (BusyBox-only
# initramfs, etc.) — a good middle tier before resorting to a from-source
# gcc build.
try_musl_cc_toolchain() {
    local triple=""
    case "$ARCH" in
        x86_64)   triple=x86_64-linux-musl ;;
        aarch64)  triple=aarch64-linux-musl ;;
        armv7l)   triple=armv7l-linux-musleabihf ;;
        armv6l)   triple=arm-linux-musleabihf ;;
        i686)     triple=i686-linux-musl ;;
        riscv64)  triple=riscv64-linux-musl ;;
        ppc64le)  triple=powerpc64le-linux-musl ;;
        s390x)    triple=s390x-linux-musl ;;
        *)        return 1 ;;
    esac
    log "Trying static musl.cc toolchain for $triple..."
    local tarball="$triple-cross.tgz"
    fetch "https://musl.cc/$tarball" "$SRC_DIR/$tarball" || return 1
    tar xzf "$SRC_DIR/$tarball" -C "$TOOLCHAIN_DIR"
    local ccbin="$TOOLCHAIN_DIR/$triple-cross/bin/$triple-gcc"
    [[ -x "$ccbin" ]] || return 1
    ln -sf "$ccbin" "$TOOLCHAIN_DIR/bin/cc"
    ln -sf "$ccbin" "$TOOLCHAIN_DIR/bin/gcc"
    log "Static toolchain ready: $ccbin"
}

ensure_c_toolchain() {
    if command -v cc &>/dev/null || command -v gcc &>/dev/null || command -v clang &>/dev/null; then
        log "C compiler present: $(command -v cc || command -v gcc || command -v clang)"
        return 0
    fi
    pkg_install "C toolchain" \
        "build-essential" "gcc make" "base-devel" "build-base" "gcc make" "base-devel" \
        && command -v cc &>/dev/null && return 0
    try_musl_cc_toolchain && return 0
    build_gcc_from_source
}
