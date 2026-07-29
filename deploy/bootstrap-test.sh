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
#   ./deploy/bootstrap-test.sh --with-cri --cni=none   # real containers, hostNetwork-only (old behavior)
#   ./deploy/bootstrap-test.sh --skip-control-plane
#   ./deploy/bootstrap-test.sh --cleanup        # tear down everything this script started
#
# CNI: real (non-hostNetwork) pods need a CNI plugin to get their own IP —
# without one, RunPodSandbox works but nothing can reach a pod except by
# sharing the host's network namespace, which defeats most of the point of
# using Kubernetes. --with-cri defaults to installing flannel (--cni=flannel):
# it's the lightest widely-used CNI (a single small daemon + the standard
# bridge/host-local CNI plugins, no separate datastore — it reads/writes pod
# CIDR allocations straight from Node objects via the "kube" subnet manager).
# --cni is a dispatch point for other plugins later; today only flannel and
# none are implemented.
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
CNI_PLUGIN=flannel

for arg in "$@"; do
    case "$arg" in
        --with-cri) WITH_CRI=1 ;;
        --skip-control-plane) SKIP_CONTROL_PLANE=1 ;;
        --cleanup) DO_CLEANUP=1 ;;
        --force-source-build) FORCE_SOURCE_BUILD=1 ;;
        --cni=*) CNI_PLUGIN="${arg#--cni=}" ;;
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
    log "Stopping flanneld (if this script started it)..."
    [[ -f "$WORK_DIR/flanneld.pid" ]] && kill "$(cat "$WORK_DIR/flanneld.pid")" 2>/dev/null || true
    log "Stopping containerd (if this script started it)..."
    [[ -f "$WORK_DIR/containerd.pid" ]] && kill "$(cat "$WORK_DIR/containerd.pid")" 2>/dev/null || true
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

# This is meant to be a single command that installs *everything*: system
# packages, the k3s control plane, containerd/runc when --with-cri is used.
# All of that needs root. Rather than making the user remember to type sudo,
# re-exec ourselves under sudo once, up front. --skip-control-plane is the
# one mode that doesn't need root (build + run nodelet against a KUBECONFIG
# you already have), so it's exempt.
if [[ "$IS_ROOT" -eq 0 && "$SKIP_CONTROL_PLANE" -eq 0 ]]; then
    if [[ -n "$SUDO" ]]; then
        exec sudo -E "$0" "$@"
    else
        die "Root is required to install system packages and the k3s control plane, and no 'sudo' is available. Re-run as root, or pass --skip-control-plane to only build/run nodelet against a KUBECONFIG you already have."
    fi
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
        "$SCRIPT_DIR/setup-control-plane.sh"
    fi
    export KUBECONFIG=/etc/rancher/k3s/k3s.yaml
}

# ─────────────────────────────────────────────────────────────────────────
# Go — only needed to build containerd/runc from source (--with-cri on an
# arch with no prebuilt containerd/runc release). Same tiering as everything
# else: package manager -> official prebuilt -> from-source bootstrap.
# ─────────────────────────────────────────────────────────────────────────

GO_VERSION=1.22.6

go_arch_map() {
    case "$ARCH" in
        x86_64)   echo amd64 ;;
        aarch64)  echo arm64 ;;
        armv7l)   echo armv6l ;;
        armv6l)   echo armv6l ;;
        i686)     echo 386 ;;
        riscv64)  echo riscv64 ;;
        ppc64le)  echo ppc64le ;;
        s390x)    echo s390x ;;
        loongarch64) echo loong64 ;;
        *)        echo "" ;;
    esac
}

go_is_new_enough() {
    command -v go &>/dev/null || return 1
    local minor; minor="$(go version | sed -n 's/.*go1\.\([0-9]\+\).*/\1/p')"
    [[ "$minor" =~ ^[0-9]+$ ]] || return 1
    (( minor >= 21 ))
}

ensure_go() {
    if go_is_new_enough; then
        log "Go present and new enough: $(go version)"
        return 0
    fi
    command -v go &>/dev/null && warn "Found $(go version) but containerd/runc need >=1.21 — looking for a newer one."

    pkg_install "go" "golang-go" "golang" "go" "go" "go" "go" \
        && go_is_new_enough && { log "Go installed via $PKG_MGR: $(go version)"; return 0; }

    local goarch; goarch="$(go_arch_map)"
    if [[ -n "$goarch" ]]; then
        log "Fetching official Go $GO_VERSION release for linux-$goarch..."
        local tarball="go$GO_VERSION.linux-$goarch.tar.gz"
        if fetch "https://go.dev/dl/$tarball" "$SRC_DIR/$tarball"; then
            rm -rf "$TOOLCHAIN_DIR/go"
            tar xzf "$SRC_DIR/$tarball" -C "$TOOLCHAIN_DIR"
            ln -sf "$TOOLCHAIN_DIR/go/bin/go" "$TOOLCHAIN_DIR/bin/go"
            go_is_new_enough && { log "Go ready: $(go version)"; return 0; }
        fi
    fi

    build_go_from_source
}

# Go's own documented from-source bootstrap: Go 1.4 (the last C-implemented
# release) builds with just a C compiler; that becomes GOROOT_BOOTSTRAP for
# an intermediate Go version, because Go >=1.21 refuses to bootstrap from
# anything older than Go 1.20; that intermediate Go then builds the final
# target version. Slow (three full Go builds) but has no binary dependency
# beyond a C compiler.
build_go_from_source() {
    log "No prebuilt Go for $ARCH — bootstrapping Go from source (three stages, slow)."
    command -v cc &>/dev/null || die "Need a C compiler to bootstrap Go and none is available."

    cd "$SRC_DIR"
    if [[ ! -x go-bootstrap-c/bin/go ]]; then
        fetch "https://dl.google.com/go/go1.4-bootstrap-20171003.tar.gz" go1.4-bootstrap.tar.gz
        rm -rf go-bootstrap-c; mkdir go-bootstrap-c
        tar xzf go1.4-bootstrap.tar.gz -C go-bootstrap-c --strip-components=1
        ( cd go-bootstrap-c/src && CGO_ENABLED=0 ./make.bash )
    fi

    local MID_VER=1.20.14
    if [[ ! -x "go-$MID_VER/bin/go" ]]; then
        fetch "https://go.dev/dl/go$MID_VER.src.tar.gz" "go$MID_VER.src.tar.gz"
        rm -rf "go-$MID_VER"; mkdir "go-$MID_VER"
        tar xzf "go$MID_VER.src.tar.gz" -C "go-$MID_VER" --strip-components=1
        ( export GOROOT_BOOTSTRAP="$SRC_DIR/go-bootstrap-c"; cd "go-$MID_VER/src" && ./make.bash )
    fi

    if [[ ! -x "go-$GO_VERSION/bin/go" ]]; then
        fetch "https://go.dev/dl/go$GO_VERSION.src.tar.gz" "go$GO_VERSION.src.tar.gz"
        rm -rf "go-$GO_VERSION"; mkdir "go-$GO_VERSION"
        tar xzf "go$GO_VERSION.src.tar.gz" -C "go-$GO_VERSION" --strip-components=1
        ( export GOROOT_BOOTSTRAP="$SRC_DIR/go-$MID_VER"; cd "go-$GO_VERSION/src" && ./make.bash )
    fi

    ln -sf "$SRC_DIR/go-$GO_VERSION/bin/go" "$TOOLCHAIN_DIR/bin/go"
    go_is_new_enough || die "Go source bootstrap finished but 'go' is still not usable."
    log "Go built from source: $(go version)"
}

# ─────────────────────────────────────────────────────────────────────────
# containerd + runc — only for --with-cri (the mock runtime needs neither).
# ─────────────────────────────────────────────────────────────────────────

fetch_containerd_runc_prebuilt() {
    local c_arch="" r_arch=""
    case "$ARCH" in
        x86_64)  c_arch=amd64;  r_arch=amd64  ;;
        aarch64) c_arch=arm64;  r_arch=arm64  ;;
        armv7l)                 r_arch=armhf  ;;
        ppc64le) c_arch=ppc64le; r_arch=ppc64le ;;
        s390x)   c_arch=s390x;  r_arch=s390x  ;;
        riscv64)                r_arch=riscv64 ;;
    esac

    if [[ -n "$c_arch" ]] && ! command -v containerd &>/dev/null; then
        local cver=1.7.23
        log "Fetching official containerd release for linux-$c_arch..."
        fetch "https://github.com/containerd/containerd/releases/download/v$cver/containerd-$cver-linux-$c_arch.tar.gz" "$SRC_DIR/containerd.tar.gz" \
            && tar xzf "$SRC_DIR/containerd.tar.gz" -C "$TOOLCHAIN_DIR" || true
    fi
    if [[ -n "$r_arch" ]] && ! command -v runc &>/dev/null; then
        local rver=1.1.14
        log "Fetching official runc release for linux-$r_arch..."
        fetch "https://github.com/opencontainers/runc/releases/download/v$rver/runc.$r_arch" "$TOOLCHAIN_DIR/bin/runc" \
            && chmod +x "$TOOLCHAIN_DIR/bin/runc" || true
    fi
    command -v containerd &>/dev/null && command -v runc &>/dev/null
}

build_containerd_runc_from_source() {
    log "No prebuilt containerd/runc for $ARCH — building both from source (needs Go)."
    ensure_go
    command -v git &>/dev/null || pkg_install git git git git git git git || true
    command -v git &>/dev/null || die "Need git to fetch containerd/runc source and couldn't get it."

    if ! command -v runc &>/dev/null; then
        cd "$SRC_DIR"
        [[ -d runc ]] || git clone --depth 1 --branch v1.1.14 https://github.com/opencontainers/runc.git
        ( cd runc && make )
        install -m 0755 runc/runc "$TOOLCHAIN_DIR/bin/runc"
    fi
    if ! command -v containerd &>/dev/null; then
        cd "$SRC_DIR"
        [[ -d containerd ]] || git clone --depth 1 --branch v1.7.23 https://github.com/containerd/containerd.git
        ( cd containerd && make )
        install -m 0755 containerd/bin/containerd "$TOOLCHAIN_DIR/bin/containerd"
        [[ -f containerd/bin/containerd-shim-runc-v2 ]] && install -m 0755 containerd/bin/containerd-shim-runc-v2 "$TOOLCHAIN_DIR/bin/"
    fi
    command -v containerd &>/dev/null && command -v runc &>/dev/null \
        || die "containerd/runc source build did not produce usable binaries."
}

ensure_container_runtime() {
    [[ "$WITH_CRI" -eq 1 ]] || return 0

    if command -v containerd &>/dev/null && command -v runc &>/dev/null; then
        log "containerd + runc already present."
    else
        pkg_install "containerd/runc" "containerd runc" "containerd runc" "containerd runc" "containerd runc" "containerd runc" "containerd runc"
        { command -v containerd &>/dev/null && command -v runc &>/dev/null; } \
            || fetch_containerd_runc_prebuilt \
            || build_containerd_runc_from_source
    fi

    mkdir -p /etc/containerd
    [[ -f /etc/containerd/config.toml ]] || containerd config default > /etc/containerd/config.toml
    if grep -qE '(docker|containerd|kubepods)' /proc/1/cgroup 2>/dev/null; then
        log "Nested container environment detected — using the native snapshotter (overlayfs can't mount here)."
        sed -i 's/snapshotter = "overlayfs"/snapshotter = "native"/' /etc/containerd/config.toml
    fi

    if ! pgrep -x containerd &>/dev/null; then
        log "Starting containerd in the background..."
        nohup containerd >"$LOG_DIR/containerd.log" 2>&1 &
        echo $! > "$WORK_DIR/containerd.pid"
        for _ in $(seq 1 15); do
            [[ -S /run/containerd/containerd.sock ]] && break
            sleep 1
        done
        [[ -S /run/containerd/containerd.sock ]] || die "containerd did not create its socket — check $LOG_DIR/containerd.log"
    fi
    export NODELET_CRI_ENDPOINT="unix:///run/containerd/containerd.sock"
}

# ─────────────────────────────────────────────────────────────────────────
# CNI networking — real pod IPs instead of hostNetwork-only.
# ─────────────────────────────────────────────────────────────────────────
# containerd's own CRI plugin invokes CNI on RunPodSandbox for any pod that
# isn't hostNetwork (nodelet already only forces hostNetwork when the Pod
# spec asks for it — see crates/nodelet/src/runtime/cri.rs). What's missing
# without this section: the CNI plugin binaries, and a network config file
# telling containerd which plugin to invoke and how. Node PodCIDR allocation
# itself needs no changes here — k3s's controller-manager runs
# --allocate-node-cidrs unconditionally, precisely so `--flannel-backend=none`
# (which setup-control-plane.sh already passes) can be paired with a
# self-managed CNI. That's exactly this setup.

CNI_BIN_DIR=/opt/cni/bin
CNI_CONF_DIR=/etc/cni/net.d

cni_go_arch_map() {   # arch names used by containernetworking/plugins and flannel-io/cni-plugin releases
    case "$ARCH" in
        x86_64)  echo amd64 ;;
        aarch64) echo arm64 ;;
        armv7l)  echo arm ;;
        ppc64le) echo ppc64le ;;
        s390x)   echo s390x ;;
        riscv64) echo riscv64 ;;
        *)       echo "" ;;
    esac
}

# Standard plugins (bridge, host-local, loopback, portmap, ...) that flannel
# (and most other CNIs) delegate to for the actual veth/bridge/IPAM work.
ensure_cni_base_plugins() {
    [[ -x "$CNI_BIN_DIR/bridge" && -x "$CNI_BIN_DIR/host-local" ]] && return 0
    mkdir -p "$CNI_BIN_DIR"

    if pkg_install "CNI plugins" "containernetworking-plugins" "containernetworking-plugins" \
            "cni-plugins" "cni-plugins" "containernetworking-plugins" "containernetworking-plugins" \
       && { [[ -x /usr/lib/cni/bridge ]] || [[ -x /usr/libexec/cni/bridge ]]; }; then
        # Distro packages install to their own dir; point ours at it.
        local distro_cni_dir; distro_cni_dir="$(dirname "$(command -v bridge 2>/dev/null || echo /usr/lib/cni/bridge)")"
        CNI_BIN_DIR="$distro_cni_dir"
        log "Using distro CNI plugins in $CNI_BIN_DIR"
        return 0
    fi

    local goarch; goarch="$(cni_go_arch_map)"
    if [[ -n "$goarch" ]]; then
        local ver=1.5.1
        log "Fetching official containernetworking/plugins release for linux-$goarch..."
        if fetch "https://github.com/containernetworking/plugins/releases/download/v$ver/cni-plugins-linux-$goarch-v$ver.tgz" "$SRC_DIR/cni-plugins.tgz"; then
            tar xzf "$SRC_DIR/cni-plugins.tgz" -C "$CNI_BIN_DIR"
            [[ -x "$CNI_BIN_DIR/bridge" ]] && { log "CNI base plugins ready in $CNI_BIN_DIR"; return 0; }
        fi
    fi

    log "No prebuilt CNI plugins for $ARCH — building from source (needs Go)."
    ensure_go
    command -v git &>/dev/null || pkg_install git git git git git git git || true
    cd "$SRC_DIR"
    [[ -d plugins ]] || git clone --depth 1 --branch v1.5.1 https://github.com/containernetworking/plugins.git
    ( cd plugins && ./build_linux.sh )
    cp plugins/bin/* "$CNI_BIN_DIR/"
    [[ -x "$CNI_BIN_DIR/bridge" ]] || die "CNI base plugin source build did not produce usable binaries."
}

ensure_flannel_binaries() {
    command -v flanneld &>/dev/null || pkg_install "flannel" "flannel" "flannel" "flannel" "flannel" "flannel" "flannel"

    local goarch; goarch="$(cni_go_arch_map)"
    if ! command -v flanneld &>/dev/null && [[ -n "$goarch" ]]; then
        local ver=0.25.6
        log "Fetching official flannel release for linux-$goarch..."
        fetch "https://github.com/flannel-io/flannel/releases/download/v$ver/flanneld-$goarch" "$TOOLCHAIN_DIR/bin/flanneld" \
            && chmod +x "$TOOLCHAIN_DIR/bin/flanneld" || true
    fi
    if ! command -v flanneld &>/dev/null; then
        log "No prebuilt flanneld for $ARCH — building from source (needs Go)."
        ensure_go
        command -v git &>/dev/null || pkg_install git git git git git git git || true
        cd "$SRC_DIR"
        [[ -d flannel ]] || git clone --depth 1 --branch v0.25.6 https://github.com/flannel-io/flannel.git
        ( cd flannel && make dist/flanneld )
        install -m 0755 flannel/dist/flanneld "$TOOLCHAIN_DIR/bin/flanneld"
    fi
    command -v flanneld &>/dev/null || die "Could not obtain a flanneld binary for $ARCH."

    # The CNI-side flannel plugin (reads /run/flannel/subnet.env, delegates to bridge).
    if [[ ! -x "$CNI_BIN_DIR/flannel" ]]; then
        if [[ -n "$goarch" ]]; then
            fetch "https://github.com/flannel-io/cni-plugin/releases/download/v1.6.0-flannel1/flannel-$goarch" "$CNI_BIN_DIR/flannel" \
                && chmod +x "$CNI_BIN_DIR/flannel" || true
        fi
        if [[ ! -x "$CNI_BIN_DIR/flannel" ]]; then
            ensure_go
            cd "$SRC_DIR"
            [[ -d cni-plugin ]] || git clone --depth 1 --branch v1.6.0-flannel1 https://github.com/flannel-io/cni-plugin.git
            ( cd cni-plugin && ./build.sh )
            install -m 0755 "cni-plugin/dist/flannel-$goarch" "$CNI_BIN_DIR/flannel"
        fi
    fi
    [[ -x "$CNI_BIN_DIR/flannel" ]] || die "Could not obtain the flannel CNI plugin binary for $ARCH."
}

write_flannel_cni_conf() {
    mkdir -p "$CNI_CONF_DIR"
    [[ -f "$CNI_CONF_DIR/10-flannel.conflist" ]] && return 0
    cat > "$CNI_CONF_DIR/10-flannel.conflist" <<EOF
{
  "name": "not-k8s-flannel",
  "cniVersion": "1.0.0",
  "plugins": [
    {
      "type": "flannel",
      "delegate": { "hairpinMode": true, "isDefaultGateway": true }
    },
    {
      "type": "portmap",
      "capabilities": { "portMappings": true }
    }
  ]
}
EOF
}

start_flanneld() {
    pgrep -x flanneld &>/dev/null && return 0
    [[ -f "$KUBECONFIG" ]] || die "flanneld needs a KUBECONFIG (control plane must be up first)."
    mkdir -p /run/flannel
    log "Starting flanneld (kube subnet manager, backend=vxlan)..."
    KUBECONFIG="$KUBECONFIG" NODE_NAME="${NODELET_NODE_NAME:-$(hostname)}" \
        nohup flanneld --kube-subnet-mgr --ip-masq \
        >"$LOG_DIR/flanneld.log" 2>&1 &
    echo $! > "$WORK_DIR/flanneld.pid"
    for _ in $(seq 1 30); do
        [[ -f /run/flannel/subnet.env ]] && break
        sleep 1
    done
    [[ -f /run/flannel/subnet.env ]] || warn "flanneld hasn't written /run/flannel/subnet.env yet — it may still be waiting on this Node's PodCIDR. Check $LOG_DIR/flanneld.log."
}

ensure_cni() {
    [[ "$WITH_CRI" -eq 1 ]] || return 0
    case "$CNI_PLUGIN" in
        none)
            log "CNI disabled (--cni=none) — pods will need hostNetwork: true to be reachable."
            return 0
            ;;
        flannel)
            ensure_cni_base_plugins
            ensure_flannel_binaries
            write_flannel_cni_conf
            start_flanneld
            ;;
        *)
            die "Unknown --cni='$CNI_PLUGIN'. Only 'flannel' and 'none' are implemented today. \
Adding another CNI (calico, cilium, ...) means: drop its plugin binaries in \
$CNI_BIN_DIR, write its config into $CNI_CONF_DIR, and start whatever daemon \
it needs — containerd and nodelet don't need to change, since containerd's \
CRI plugin drives CNI generically and nodelet only decides host-vs-pod \
networking per Pod spec."
            ;;
    esac
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
ensure_container_runtime
ensure_cni
run_and_verify
