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
# initramfs, etc.). A generic glibc compiler is never accepted for the Rust
# target: it would put the host libc back into an otherwise static binary.
MUSL_CC_TRIPLE() {
    case "$ARCH" in
        x86_64)  echo x86_64-linux-musl ;;
        aarch64) echo aarch64-linux-musl ;;
        armv7l)  echo armv7l-linux-musleabihf ;;
        armv6l)  echo arm-linux-musleabihf ;;
        i686)    echo i686-linux-musl ;;
        riscv64) echo riscv64-linux-musl ;;
        ppc64le) echo powerpc64le-linux-musl ;;
        s390x)   echo s390x-linux-musl ;;
        *)        echo "" ;;
    esac
}

try_musl_cc_toolchain() {
    local triple="$(MUSL_CC_TRIPLE)"
    [[ -n "$triple" ]] || return 1
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

find_musl_cc() {
    local triple="$(MUSL_CC_TRIPLE)" candidate machine resolved
    local -a candidates=(
        "$TOOLCHAIN_DIR/$triple-cross/bin/$triple-gcc"
        "$TOOLCHAIN_DIR/bin/gcc"
        "$TOOLCHAIN_DIR/bin/cc"
        "$(command -v musl-gcc 2>/dev/null || true)"
        "$(command -v "${triple}-gcc" 2>/dev/null || true)"
        "$(command -v gcc 2>/dev/null || true)"
        "$(command -v cc 2>/dev/null || true)"
    )
    for candidate in "${candidates[@]}"; do
        [[ -x "$candidate" ]] || continue
        machine="$($candidate -dumpmachine 2>/dev/null || true)"
        resolved="$(readlink -f "$candidate" 2>/dev/null || true)"
        if [[ "$machine" == *musl* || "${candidate##*/}" == musl-gcc || "$resolved" == *musl* ]]; then
            printf '%s\n' "$candidate"
            return 0
        fi
    done
    return 1
}

configure_musl_cargo() {
    local compiler="$1" target key upper cc_var linker_var rustflags_var flags
    target="$(RUSTUP_TARGET_MAP)"
    key="${target//-/_}"
    upper="$(printf '%s' "$key" | tr '[:lower:]' '[:upper:]')"
    cc_var="CC_$key"
    linker_var="CARGO_TARGET_${upper}_LINKER"
    rustflags_var="CARGO_TARGET_${upper}_RUSTFLAGS"
    printf -v "$cc_var" '%s' "$compiler"
    printf -v "$linker_var" '%s' "$compiler"
    flags="${!rustflags_var:-}"
    [[ "$flags" == *'target-feature=+crt-static'* ]] || flags="${flags:+$flags }-C target-feature=+crt-static"
    printf -v "$rustflags_var" '%s' "$flags"
    export "$cc_var" "$linker_var" "$rustflags_var"
    export MUSL_C_COMPILER="$compiler" MUSL_RUST_TARGET="$target"
    log "Using static musl compiler $compiler for Rust target $target"
}

ensure_c_toolchain() {
    local compiler
    if compiler="$(find_musl_cc)"; then
        configure_musl_cargo "$compiler"
        return 0
    fi
    pkg_install "musl C toolchain" \
        "musl-tools musl-dev" "musl-gcc musl-libc-devel" "musl" "musl-dev gcc" "musl-devel gcc" "musl-devel" \
        && compiler="$(find_musl_cc)" && { configure_musl_cargo "$compiler"; return 0; }
    if try_musl_cc_toolchain && compiler="$(find_musl_cc)"; then
        configure_musl_cargo "$compiler"
        return 0
    fi
    die "No usable musl C compiler is available for $ARCH. Refusing the generic glibc compiler fallback because it would make the deployed Rust binaries depend on the host's glibc. Install a musl development toolchain or make the matching musl.cc toolchain reachable."
}
