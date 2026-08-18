# lib/toolchain-rust.sh — Rust, prebuilt only. rustc cannot be bootstrapped
# from nothing but a C compiler — every path to a working rustc starts from
# an existing rustc binary (rustup's prebuilts cover the realistic
# embedded/edge architecture set). If none of those match, this says so
# explicitly and points at the only real alternative (mrustc) instead of
# pretending to solve it.
#
# NOTE: this whole file becomes optional once GitHub Actions builds nodelet
# centrally (see nodelet-build.sh) — an on-device install only needs
# ensure_rust() at all when no prebuilt nodelet binary is available for its
# arch/libc, so this is naturally the first thing later CI work can make
# most devices skip entirely.

RUSTUP_TARGET_MAP() {
    case "$ARCH" in
        x86_64)      echo x86_64-unknown-linux-gnu ;;
        aarch64)     echo aarch64-unknown-linux-gnu ;;
        armv7l)      echo armv7-unknown-linux-gnueabihf ;;
        armv6l)      echo arm-unknown-linux-gnueabihf ;;
        i686)        echo i686-unknown-linux-gnu ;;
        riscv64)     echo riscv64gc-unknown-linux-gnu ;;
        ppc64le)     echo powerpc64le-unknown-linux-gnu ;;
        s390x)       echo s390x-unknown-linux-gnu ;;
        loongarch64) echo loongarch64-unknown-linux-gnu ;;
        *)           echo "" ;;
    esac
}

# Distro-packaged Rust is very often too old: this workspace's deps (kube,
# tonic, edition-2021 MSRVs) and even the Cargo.lock format itself need a
# fairly recent toolchain. Debian bookworm ships rustc 1.63 from 2022, which
# can't even parse a v4 lockfile. So: any cargo older than this is treated
# as "not found" and we fall through to rustup instead of failing later
# with a confusing lockfile/MSRV error. This number is the actual MSRV of
# the exact dependency versions pinned in Cargo.lock (kube 4.0.0 / tonic
# 0.14.6 currently require rustc 1.88) — bump it if `cargo build` ever
# fails with "rustc X is not supported by the following packages" even
# though this check passed; that means a dependency bump raised the MSRV
# past what this constant knows about.
MIN_CARGO_MINOR=88

cargo_is_new_enough() {
    command -v cargo &>/dev/null || return 1
    local ver minor
    ver="$(cargo --version | awk '{print $2}')"   # e.g. 1.65.0
    minor="$(echo "$ver" | cut -d. -f2)"
    [[ "$minor" =~ ^[0-9]+$ ]] || return 1
    (( minor >= MIN_CARGO_MINOR ))
}

ensure_rust() {
    if cargo_is_new_enough; then
        log "Rust present and new enough: $(cargo --version)"
        return 0
    fi
    if command -v cargo &>/dev/null; then
        warn "Found $(cargo --version) but this project needs >=1.$MIN_CARGO_MINOR — looking for a newer one."
    fi

    pkg_install "rust" "cargo rustc" "cargo rustc" "rust" "cargo" "cargo rustc" "rust" \
        && cargo_is_new_enough && { log "Rust installed via $PKG_MGR: $(cargo --version)"; return 0; }

    local target; target="$(RUSTUP_TARGET_MAP)"
    [[ -n "$target" ]] || die "No known rustup target for arch '$ARCH'. \
There is no way to build rustc from nothing but a C compiler for an \
unsupported architecture — the only real path is mrustc \
(https://github.com/thepowersgang/mrustc), a from-scratch multi-stage \
bootstrap that is out of scope for a single test script. If you hit this, \
please open an issue with your arch — we may be able to add a cross build."

    log "Installing Rust via rustup (target: $target)..."
    export RUSTUP_HOME="$TOOLCHAIN_DIR/rustup"
    export CARGO_HOME="$TOOLCHAIN_DIR/cargo"
    fetch "https://sh.rustup.rs" "$SRC_DIR/rustup-init.sh"
    sh "$SRC_DIR/rustup-init.sh" -y --default-toolchain stable --target "$target" --no-modify-path \
        || die "rustup could not install a toolchain for $target. This architecture has no \
official prebuilt rustc — see the mrustc note above."
    export PATH="$CARGO_HOME/bin:$PATH"
    command -v cargo &>/dev/null || die "rustup ran but cargo is still not on PATH."
    log "Rust installed via rustup: $(cargo --version)"
}
