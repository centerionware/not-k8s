#!/usr/bin/env bash
# bootstrap-test.sh — single-file, self-contained deployment for testing not-k8s
# on *any* Linux box: any distro, any package manager (or none), any CPU
# architecture.
#
# Strategy, per dependency, cheapest/fastest first:
#   1. Already installed?                              -> use it.
#   2. Native package manager (apt/dnf/yum/pacman/apk/
#      zypper/xbps/emerge/pkg)?                         -> use it.
#   3. Official upstream prebuilt release for this
#      exact OS/arch (rustup, protoc, Go)?              -> download it.
#   4. Static prebuilt cross toolchain (musl.cc) that
#      *runs* on this arch, for a plain C compiler?     -> download it.
#   5. Build the dependency from *source* (gcc, Go).     -> compile it here.
#      This is the "even if it means building gcc or go" tier: slow
#      (30-90+ min per component) but has no external binary dependency
#      beyond a working C compiler and this shell.
#
# Rust is the one hard wall: rustc cannot be bootstrapped from nothing but a
# C compiler — every path to a working rustc starts from an existing rustc
# binary (rustup's prebuilts cover the realistic embedded/edge architecture
# set: x86_64, aarch64, armv7, arm, i686, riscv64gc, powerpc64le, s390x,
# loongarch64). If none of those match, this script says so explicitly and
# points at the only real alternative (mrustc) instead of pretending to
# solve it.
#
# Usage:
#   ./deploy/bootstrap-test.sh                  # mock runtime, k3s control plane, demo pod
#   ./deploy/bootstrap-test.sh --with-cri       # also build+use the real containerd/CRI runtime
#   ./deploy/bootstrap-test.sh --skip-control-plane
#   ./deploy/bootstrap-test.sh --cleanup        # tear down everything this script started
#
set -euo pipefail

# ─────────────────────────────────────────────────────────────────────────
# Config & flags
# ─────────────────────────────────────────────────────────────────────────

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

WORK_DIR="${NOTK8S_WORK_DIR:-$REPO_ROOT/.bootstrap}"
TOOLCHAIN_DIR="$WORK_DIR/toolchain"
SRC_DIR="$WORK_DIR/src"
LOG_DIR="$WORK_DIR/logs"

WITH_CRI=0
SKIP_CONTROL_PLANE=0
DO_CLEANUP=0
FORCE_SOURCE_BUILD=0

for arg in "$@"; do
    case "$arg" in
        --with-cri) WITH_CRI=1 ;;
        --skip-control-plane) SKIP_CONTROL_PLANE=1 ;;
        --cleanup) DO_CLEANUP=1 ;;
        --force-source-build) FORCE_SOURCE_BUILD=1 ;;
        -h|--help)
            grep '^#' "$0" | sed -e 's/^# \{0,1\}//' -e '1,3d'
            exit 0
            ;;
        *)
            echo "Unknown flag: $arg" >&2
            exit 1
            ;;
    esac
done

mkdir -p "$WORK_DIR" "$TOOLCHAIN_DIR" "$SRC_DIR" "$LOG_DIR"

log()  { printf '\033[1;34m==>\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33m==> WARNING:\033[0m %s\n' "$*" >&2; }
die()  { printf '\033[1;31m==> FATAL:\033[0m %s\n' "$*" >&2; exit 1; }

# ─────────────────────────────────────────────────────────────────────────
# Cleanup mode
# ─────────────────────────────────────────────────────────────────────────

if [[ "$DO_CLEANUP" -eq 1 ]]; then
    log "Stopping nodelet (if running via this script's pidfile)..."
    [[ -f "$WORK_DIR/nodelet.pid" ]] && kill "$(cat "$WORK_DIR/nodelet.pid")" 2>/dev/null || true
    if command -v k3s-uninstall.sh &>/dev/null; then
        log "Uninstalling k3s..."
        sudo k3s-uninstall.sh || true
    fi
    log "Removing $WORK_DIR (downloaded toolchains, build dirs, logs)..."
    rm -rf "$WORK_DIR"
    log "Cleanup done. 'cargo clean' if you also want target/ gone."
    exit 0
fi

# ─────────────────────────────────────────────────────────────────────────
# OS / arch / package-manager detection
# ─────────────────────────────────────────────────────────────────────────

OS="$(uname -s)"
[[ "$OS" == "Linux" ]] || die "This script targets Linux only (found: $OS)."

ARCH_RAW="$(uname -m)"
case "$ARCH_RAW" in
    x86_64|amd64)           ARCH=x86_64 ;;
    aarch64|arm64)          ARCH=aarch64 ;;
    armv7l|armv7)           ARCH=armv7l ;;
    armv6l)                 ARCH=armv6l ;;
    i686|i386)              ARCH=i686 ;;
    riscv64)                ARCH=riscv64 ;;
    ppc64le)                ARCH=ppc64le ;;
    s390x)                  ARCH=s390x ;;
    loongarch64)            ARCH=loongarch64 ;;
    *)                      ARCH="$ARCH_RAW" ;;
esac

PKG_MGR=unknown
if   command -v apt-get &>/dev/null; then PKG_MGR=apt
elif command -v dnf     &>/dev/null; then PKG_MGR=dnf
elif command -v yum     &>/dev/null; then PKG_MGR=yum
elif command -v pacman  &>/dev/null; then PKG_MGR=pacman
elif command -v apk     &>/dev/null; then PKG_MGR=apk
elif command -v zypper  &>/dev/null; then PKG_MGR=zypper
elif command -v xbps-install &>/dev/null; then PKG_MGR=xbps
elif command -v emerge  &>/dev/null; then PKG_MGR=emerge
elif command -v pkg     &>/dev/null; then PKG_MGR=pkg
fi

IS_ROOT=0
[[ "$EUID" -eq 0 ]] && IS_ROOT=1
SUDO=""
if [[ "$IS_ROOT" -eq 0 ]]; then
    command -v sudo &>/dev/null && SUDO="sudo"
fi

log "OS=$OS  arch=$ARCH_RAW (normalized: $ARCH)  pkg_mgr=$PKG_MGR  root=$IS_ROOT"

# pkg_install <logical-name> <apt-pkg> <dnf-pkg> <pacman-pkg> <apk-pkg> <zypper-pkg> <xbps-pkg>
# Best-effort: returns 0 on apparent success, 1 if no package manager could be used.
pkg_install() {
    local name="$1" apt="$2" dnf="$3" pacman="$4" apk="$5" zypper="$6" xbps="$7"
    [[ "$FORCE_SOURCE_BUILD" -eq 1 ]] && return 1
    log "Trying to install '$name' via $PKG_MGR..."
    case "$PKG_MGR" in
        apt)
            $SUDO apt-get update -qq -y >>"$LOG_DIR/pkg.log" 2>&1 || true
            $SUDO apt-get install -qq -y $apt >>"$LOG_DIR/pkg.log" 2>&1
            ;;
        dnf)    $SUDO dnf install -y -q $dnf >>"$LOG_DIR/pkg.log" 2>&1 ;;
        yum)    $SUDO yum install -y -q $dnf >>"$LOG_DIR/pkg.log" 2>&1 ;;
        pacman) $SUDO pacman -Sy --noconfirm --needed $pacman >>"$LOG_DIR/pkg.log" 2>&1 ;;
        apk)    $SUDO apk add --no-cache $apk >>"$LOG_DIR/pkg.log" 2>&1 ;;
        zypper) $SUDO zypper --non-interactive install $zypper >>"$LOG_DIR/pkg.log" 2>&1 ;;
        xbps)   $SUDO xbps-install -Sy $xbps >>"$LOG_DIR/pkg.log" 2>&1 ;;
        *)      return 1 ;;
    esac
}

fetch() { # fetch <url> <output-path>
    if command -v curl &>/dev/null; then
        curl -fsSL --retry 3 -o "$2" "$1"
    elif command -v wget &>/dev/null; then
        wget -q -O "$2" "$1"
    else
        die "Neither curl nor wget is available, and none could be installed."
    fi
}

# curl/wget themselves have to come from *somewhere* — every distro package
# manager ships one of them, so this is the one dependency we require the
# package manager (or the base image) to already provide.
if ! command -v curl &>/dev/null && ! command -v wget &>/dev/null; then
    pkg_install curl curl curl curl curl curl curl || true
fi
command -v curl &>/dev/null || command -v wget &>/dev/null \
    || die "No curl/wget and no usable package manager — cannot fetch anything. Install curl manually first."

export PATH="$TOOLCHAIN_DIR/bin:$PATH"

# ─────────────────────────────────────────────────────────────────────────
# Tier 5 fallback: build gcc (+ binutils) from pure source
# ─────────────────────────────────────────────────────────────────────────
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

# ─────────────────────────────────────────────────────────────────────────
# Rust — prebuilt only. See header comment for why there is no source path.
# ─────────────────────────────────────────────────────────────────────────

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
# with a confusing lockfile/MSRV error.
MIN_CARGO_MINOR=78

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

# ─────────────────────────────────────────────────────────────────────────
# protoc — only needed for --with-cri
# ─────────────────────────────────────────────────────────────────────────

ensure_protoc() {
    [[ "$WITH_CRI" -eq 1 ]] || return 0
    if command -v protoc &>/dev/null; then
        log "protoc present: $(protoc --version)"
        return 0
    fi

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

# ─────────────────────────────────────────────────────────────────────────
# k3s control plane
# ─────────────────────────────────────────────────────────────────────────
# k3s upstream only publishes binaries for amd64/arm64/armhf/s390x. That's a
# real, current limitation of using k3s as the control plane on truly
# exotic hardware — not something a shell script can paper over without
# building the whole of upstream Kubernetes + etcd/kine from source (hours,
# many GB, and no guarantee of success on an untested arch). We detect and
# say so rather than pretend.

k3s_supports_arch() {
    case "$ARCH" in
        x86_64|aarch64|armv7l|s390x) return 0 ;;
        *) return 1 ;;
    esac
}

setup_control_plane() {
    [[ "$SKIP_CONTROL_PLANE" -eq 1 ]] && { log "Skipping control plane (--skip-control-plane)."; return 0; }

    if ! k3s_supports_arch; then
        warn "k3s has no upstream release for arch '$ARCH'. Known limitation — see README."
        warn "Skipping automatic control-plane setup. Run k3s from source or use k0s/another"
        warn "distro's build for this arch, then point KUBECONFIG at it and re-run with"
        warn "--skip-control-plane."
        return 0
    fi

    if command -v k3s &>/dev/null && systemctl is-active --quiet k3s 2>/dev/null; then
        log "k3s already installed and running."
    else
        [[ "$IS_ROOT" -eq 1 ]] || die "Setting up the k3s control plane needs root. Re-run with sudo, or pass --skip-control-plane if you already have a KUBECONFIG."
        "$SCRIPT_DIR/setup-control-plane.sh"
    fi
    export KUBECONFIG=/etc/rancher/k3s/k3s.yaml
}

# ─────────────────────────────────────────────────────────────────────────
# Build nodelet
# ─────────────────────────────────────────────────────────────────────────

build_nodelet() {
    cd "$REPO_ROOT"
    local features=()
    if [[ "$WITH_CRI" -eq 1 ]]; then
        ensure_protoc
        features=(--features cri)
    fi
    log "Building nodelet (cargo build --release ${features[*]:-})..."
    cargo build --release "${features[@]}"
    [[ -x "$REPO_ROOT/target/release/nodelet" ]] || die "Build finished but binary not found."
    log "nodelet built: $REPO_ROOT/target/release/nodelet"
}

# ─────────────────────────────────────────────────────────────────────────
# Run it
# ─────────────────────────────────────────────────────────────────────────

run_and_verify() {
    export KUBECONFIG="${KUBECONFIG:-/etc/rancher/k3s/k3s.yaml}"
    if [[ ! -f "$KUBECONFIG" ]]; then
        warn "No KUBECONFIG at $KUBECONFIG — control plane wasn't set up (see above)."
        warn "nodelet needs an apiserver to register against; stopping before running it."
        return 0
    fi

    export NODELET_RUNTIME="mock"
    [[ "$WITH_CRI" -eq 1 ]] && NODELET_RUNTIME="cri"

    log "Starting nodelet (runtime=$NODELET_RUNTIME) in the background..."
    nohup "$SCRIPT_DIR/run-nodelet.sh" >"$LOG_DIR/nodelet.log" 2>&1 &
    echo $! > "$WORK_DIR/nodelet.pid"
    sleep 3

    log "Waiting for the node to register..."
    for i in $(seq 1 20); do
        if kubectl get nodes --no-headers 2>/dev/null | grep -q .; then
            break
        fi
        sleep 2
    done
    kubectl get nodes -o wide || warn "kubectl get nodes failed — check $LOG_DIR/nodelet.log"

    log "Applying demo pod..."
    kubectl apply -f "$REPO_ROOT/deploy/demo-pod.yaml"
    sleep 3
    kubectl get pods -o wide

    log "Done. Tail logs with: tail -f $LOG_DIR/nodelet.log"
    log "Tear everything down with: $0 --cleanup"
}

# ─────────────────────────────────────────────────────────────────────────
# Main
# ─────────────────────────────────────────────────────────────────────────

log "not-k8s bootstrap-test: isolated single-script deployment"
ensure_c_toolchain
ensure_rust
setup_control_plane
build_nodelet
run_and_verify
