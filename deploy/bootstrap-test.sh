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
# nodelet itself is installed as a real, persistent, auto-restarting service
# (systemd, or OpenRC if that's what this system has — k3s's own installer
# doesn't support anything else, so nothing running k3s should lack both),
# the same treatment k3s already gets — not just started in the foreground
# and left to die with this script's terminal.
#
# Usage:
#   ./deploy/bootstrap-test.sh                  # mock runtime, k3s control plane, demo pod
#   ./deploy/bootstrap-test.sh --with-cri       # also build+use the real containerd/CRI runtime
#   ./deploy/bootstrap-test.sh --with-cri --cni=none   # real containers, hostNetwork-only (old behavior)
#   ./deploy/bootstrap-test.sh --with-cri --ip-family=ipv4     # force v4-only
#   ./deploy/bootstrap-test.sh --with-cri --lb-method=round-robin
#   ./deploy/bootstrap-test.sh --skip-control-plane
#   ./deploy/bootstrap-test.sh --keep-build-tools   # skip the end-of-run toolchain cleanup (faster re-runs)
#   ./deploy/bootstrap-test.sh --cleanup        # stop the deployment + build-tool cleanup (keeps runtime pkgs/k3s for next time)
#   ./deploy/bootstrap-test.sh --uninstall      # full teardown: also k3s, containerd/runc, CNI/flannel, nftables
#   ./deploy/bootstrap-test.sh --uninstall --force  # same, but by name — ignores tracking entirely
#
# Footprint: this is meant to end with only nodelet's binary and whatever it
# needs *running* left behind on the device — not a permanently-installed
# Rust/C/Go toolchain. Once the build finishes (and, if --with-cri pulled in
# Go to build containerd/runc/CNI plugins/flannel from source, once that's
# done too), the script uninstalls every build-only package IT installed
# fresh — never something that was already on the system — deletes all
# download/build scratch, and wipes the entire `target/` build cache after
# copying the final binary to `bin/nodelet` (the stable path `run-nodelet.sh`
# looks for). Runtime pieces (containerd, runc, flanneld, the CNI plugins,
# nftables, k3s) are never touched by this — the cluster still needs them
# running. Pass --keep-build-tools to skip this and leave everything in
# place, e.g. while iterating on the script itself.
#
# --cleanup vs --uninstall: --cleanup stops what a run started (nodelet,
# flanneld, containerd, the nft Service table) and removes k3s + this
# script's own scratch — enough to start clean, but keeps runtime packages
# installed so the next run is fast. --uninstall goes further: k3s's
# data/config, containerd/runc's state and binaries, and all CNI/flannel
# config/binaries too — but, same rule as everywhere else in this script,
# only for what it actually installed. If containerd/runc or the CNI/flannel
# setup predate this script (e.g. you already had Docker's containerd), they
# and their state/data are left completely untouched.
#
# --force (only meaningful with --uninstall): the ownership tracking
# --uninstall relies on (pkg_installs.log, the flannel CNI plugin binary's
# presence) only exists for runs of *this* version of the script. If an
# older version left a machine dirty — no tracking log at all, e.g. before
# this flag existed — plain --uninstall will correctly conclude it owns
# nothing and do nothing useful. --force skips every ownership check and
# removes k3s, containerd/runc, CNI plugins, flannel, and nftables by name,
# whether or not this exact script installed them. This is real fallout, not
# a preference: it can remove packages/config you set up yourself outside
# this project if they happen to share these names — use it when you know
# the machine's state is this project's mess to clean up, not a shared box.
#
# --ip-family: auto (default) | ipv4 | ipv6 | dual. auto detects what the
# node actually has (both stacks -> dual, one stack -> that one) and uses the
# same result for k3s's --cluster-cidr/--service-cidr, flannel, and nodelet's
# Service proxy, so all three agree. --lb-method: random (default; matches
# how kube-proxy iptables mode behaves) | round-robin | source-hash (sticky
# per client IP — also used automatically for any Service that sets
# `sessionAffinity: ClientIP`, regardless of this default).
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
# Services (ClusterIP/NodePort) need more than a pod IP — kube-proxy's job of
# turning a virtual ClusterIP into a real backend has to happen somewhere.
# nodelet does it itself with nftables (crates/nodelet/src/svc.rs, watches
# Services+Endpoints, no separate process). This script just makes sure `nft`
# is installed and that bridged pod traffic reaches the host's netfilter
# tables (br_netfilter) so the DNAT rules actually see it.
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
DO_UNINSTALL=0
FORCE_UNINSTALL=0
FORCE_SOURCE_BUILD=0
CNI_PLUGIN=flannel
IP_FAMILY=auto
LB_METHOD=random
KEEP_BUILD_TOOLS=0

for arg in "$@"; do
    case "$arg" in
        --with-cri) WITH_CRI=1 ;;
        --skip-control-plane) SKIP_CONTROL_PLANE=1 ;;
        --cleanup) DO_CLEANUP=1 ;;
        --uninstall) DO_UNINSTALL=1 ;;
        --force) FORCE_UNINSTALL=1 ;;
        --force-source-build) FORCE_SOURCE_BUILD=1 ;;
        --cni=*) CNI_PLUGIN="${arg#--cni=}" ;;
        --ip-family=*) IP_FAMILY="${arg#--ip-family=}" ;;
        --lb-method=*) LB_METHOD="${arg#--lb-method=}" ;;
        --keep-build-tools) KEEP_BUILD_TOOLS=1 ;;
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

mkdir -p "$WORK_DIR" "$TOOLCHAIN_DIR" "$TOOLCHAIN_DIR/bin" "$SRC_DIR" "$LOG_DIR"

log()  { printf '\033[1;34m==>\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33m==> WARNING:\033[0m %s\n' "$*" >&2; }
die()  { printf '\033[1;31m==> FATAL:\033[0m %s\n' "$*" >&2; exit 1; }

[[ "$FORCE_UNINSTALL" -eq 1 && "$DO_UNINSTALL" -ne 1 ]] \
    && die "--force only means something alongside --uninstall (it makes --uninstall remove everything by name, not just what this script tracked installing)."

# ─────────────────────────────────────────────────────────────────────────
# OS / arch / package-manager detection
# ─────────────────────────────────────────────────────────────────────────
#
# --cleanup is handled by full_cleanup(), defined further down and invoked
# from the Main section — not as an early exit here — because it needs
# $PKG_MGR/$SUDO (detected below) and uninstall_tracked_build_packages()
# (defined alongside the other build functions) to remove exactly the
# build-only packages this script installed, the same as the automatic
# end-of-run cleanup a normal install does.
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

# ─────────────────────────────────────────────────────────────────────────
# IP family resolution — decided once, here, and handed to every consumer
# (k3s's --cluster-cidr/--service-cidr, flannel's net-conf, and nodelet's
# NODELET_IP_FAMILY) so they can't disagree with each other.
#
# Checks for an actual default route in each family — deliberately the same
# thing flannel itself checks when picking its interface ("failed to get
# default v6 interface: Unable to find default v6 route"). An earlier
# version of this used a UDP socket *bind* test instead, which only proves
# the address family's socket API works — nearly every modern kernel has
# IPv6 compiled in and enabled regardless of whether there's any real IPv6
# connectivity, so that test passed on a machine with no default v6 route
# at all. Result, confirmed for real: auto picked "dual", flannel got a
# net-conf.json telling it to also handle IPv6, and it crash-looped forever
# since it could never find a v6 interface to use. A route-table check is
# the actual thing that matters, and it's exactly what was missing.
# ─────────────────────────────────────────────────────────────────────────

detect_ipv4() {
    if command -v ip &>/dev/null; then
        ip -4 route show default 2>/dev/null | grep -q .
    else
        # No iproute2 to check a real route table. IPv4 is near-universal,
        # so a bind test (proves the socket API works, not real
        # connectivity — weaker, but the false-positive direction here is
        # low-risk) is a reasonable fallback.
        command -v python3 &>/dev/null \
            && python3 -c 'import socket; socket.socket(socket.AF_INET, socket.SOCK_DGRAM).bind(("0.0.0.0", 0))' 2>/dev/null
    fi
}
detect_ipv6() {
    if command -v ip &>/dev/null; then
        ip -6 route show default 2>/dev/null | grep -q .
    else
        # No route table to check, and unlike IPv4, a false positive here
        # is exactly what caused flannel to crash-loop — default to "no
        # v6" rather than trust a weaker test. Worst case is ipv4-only when
        # dual-stack would actually have worked; that's a far better
        # failure mode than a service that can never start.
        false
    fi
}

case "$IP_FAMILY" in
    auto)
        v4=0; v6=0
        detect_ipv4 && v4=1
        detect_ipv6 && v6=1
        if [[ "$v4" -eq 1 && "$v6" -eq 1 ]]; then IP_FAMILY=dual
        elif [[ "$v6" -eq 1 ]]; then IP_FAMILY=ipv6
        else IP_FAMILY=ipv4
        fi
        ;;
    ipv4|ipv6|dual) ;;
    *) die "Unknown --ip-family='$IP_FAMILY' (want 'auto', 'ipv4', 'ipv6', or 'dual')." ;;
esac
log "IP family: $IP_FAMILY"

# CIDRs match Rancher's own documented k3s dual-stack example, so anyone
# comparing against upstream k3s docs sees familiar numbers.
IPV4_CLUSTER_CIDR="10.42.0.0/16"
IPV4_SERVICE_CIDR="10.43.0.0/16"
IPV6_CLUSTER_CIDR="fd00:42::/48"
IPV6_SERVICE_CIDR="fd00:43::/112"
case "$IP_FAMILY" in
    ipv4) CLUSTER_CIDR="$IPV4_CLUSTER_CIDR"; SERVICE_CIDR="$IPV4_SERVICE_CIDR" ;;
    ipv6) CLUSTER_CIDR="$IPV6_CLUSTER_CIDR"; SERVICE_CIDR="$IPV6_SERVICE_CIDR" ;;
    dual) CLUSTER_CIDR="$IPV4_CLUSTER_CIDR,$IPV6_CLUSTER_CIDR"; SERVICE_CIDR="$IPV4_SERVICE_CIDR,$IPV6_SERVICE_CIDR" ;;
esac
export NOTK8S_CLUSTER_CIDR="$CLUSTER_CIDR" NOTK8S_SERVICE_CIDR="$SERVICE_CIDR"

case "$LB_METHOD" in
    random|round-robin|source-hash) ;;
    *) die "Unknown --lb-method='$LB_METHOD' (want 'random', 'round-robin', or 'source-hash')." ;;
esac

# pkg_install <logical-name> <apt-pkg> <dnf-pkg> <pacman-pkg> <apk-pkg> <zypper-pkg> <xbps-pkg>
# Best-effort: returns 0 on apparent success, 1 if no package manager could be used.
# Every successful install is appended to $WORK_DIR/pkg_installs.log as
# "<pkgmgr>|<logical-name>|<packages>" — this is what makes the end-of-run
# footprint cleanup possible: only packages *this script* actually installed
# get recorded, so cleanup can uninstall exactly those and leave anything
# that was already on the system untouched. Never logs failures.
pkg_install() {
    local name="$1" apt="$2" dnf="$3" pacman="$4" apk="$5" zypper="$6" xbps="$7"
    [[ "$FORCE_SOURCE_BUILD" -eq 1 ]] && return 1
    log "Trying to install '$name' via $PKG_MGR..."
    local pkgs="" ok=1
    case "$PKG_MGR" in
        apt)
            pkgs="$apt"
            $SUDO apt-get update -qq -y >>"$LOG_DIR/pkg.log" 2>&1 || true
            $SUDO apt-get install -qq -y $apt >>"$LOG_DIR/pkg.log" 2>&1 && ok=0 || ok=1
            ;;
        dnf)    pkgs="$dnf";    $SUDO dnf install -y -q $dnf >>"$LOG_DIR/pkg.log" 2>&1 && ok=0 || ok=1 ;;
        yum)    pkgs="$dnf";    $SUDO yum install -y -q $dnf >>"$LOG_DIR/pkg.log" 2>&1 && ok=0 || ok=1 ;;
        pacman) pkgs="$pacman"; $SUDO pacman -Sy --noconfirm --needed $pacman >>"$LOG_DIR/pkg.log" 2>&1 && ok=0 || ok=1 ;;
        apk)    pkgs="$apk";    $SUDO apk add --no-cache $apk >>"$LOG_DIR/pkg.log" 2>&1 && ok=0 || ok=1 ;;
        zypper) pkgs="$zypper"; $SUDO zypper --non-interactive install $zypper >>"$LOG_DIR/pkg.log" 2>&1 && ok=0 || ok=1 ;;
        xbps)   pkgs="$xbps";   $SUDO xbps-install -Sy $xbps >>"$LOG_DIR/pkg.log" 2>&1 && ok=0 || ok=1 ;;
        *)      return 1 ;;
    esac
    [[ "$ok" -eq 0 ]] && echo "$PKG_MGR|$name|$pkgs" >> "$WORK_DIR/pkg_installs.log"
    return "$ok"
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

# Generic persistent-service installer: systemd (Restart=always, enabled on
# boot) -> OpenRC (supervise-daemon, respawn, added to boot) -> a
# self-restarting background loop + cron @reboot as a last resort, clearly
# logged as not a real service rather than silently accepted as good enough.
# Learned the hard way: nodelet was originally just `nohup`'d here too, which
# meant it silently died on any crash/reboot/terminal-close with nothing to
# bring it back — the same bug class applies to anything else this script
# starts and runs long-term (flanneld, and containerd when we start it
# ourselves rather than using its own package's service).
#
# $1 name, $2 description, $3 exec command (a single string — run through
# `sh -c` in every tier so it doesn't need re-parsing per init system) —
# MUST use an absolute path for the binary, never a bare command name:
# systemd/OpenRC services get a fresh, minimal PATH that won't include
# wherever this script's own PATH additions put a fetched/built binary
# ($TOOLCHAIN_DIR/bin), so a bare name resolves fine in this script's own
# shell and then fails with "not found" (exit 127) the moment the service
# manager actually runs it — this happened for real with flanneld, use
# "$(command -v the-binary)" at the call site like that fix does,
# $4 extra After=/depend() unit name or "" for none, $@ (from $5) KEY=VALUE
# environment pairs (zero or more).
install_supervised_service() {
    local name="$1" desc="$2" exec_cmd="$3" after="$4"
    shift 4
    local envs=("$@") env_systemd="" env_shell="" kv
    for kv in "${envs[@]}"; do
        env_systemd+="Environment=$kv"$'\n'
        env_shell+="export $kv"$'\n'
    done

    if command -v systemctl &>/dev/null; then
        log "Installing $name as a systemd service (Restart=always, enabled on boot)..."
        cat > "/etc/systemd/system/$name.service" <<EOF
[Unit]
Description=$desc
After=network-online.target${after:+ $after}
Wants=network-online.target${after:+ $after}

[Service]
Type=simple
ExecStart=/bin/sh -c '$exec_cmd'
Restart=always
RestartSec=5s
$env_systemd
[Install]
WantedBy=multi-user.target
EOF
        systemctl daemon-reload
        systemctl enable --now "$name.service"
        sleep 2
        systemctl is-active --quiet "$name.service" \
            || warn "$name.service didn't come up cleanly — check: journalctl -u $name -n 50"
    elif command -v rc-service &>/dev/null && command -v rc-update &>/dev/null; then
        log "Installing $name as an OpenRC service (supervised, auto-restart, added to boot)..."
        cat > "/etc/init.d/$name" <<EOF
#!/sbin/openrc-run
description="$desc"

$env_shell
supervisor="supervise-daemon"
command="/bin/sh"
command_args="-c '$exec_cmd'"
respawn_max=0
respawn_delay=5

depend() {
    need net
$( [[ -n "$after" ]] && echo "    after ${after%.service}" )
}
EOF
        chmod +x "/etc/init.d/$name"
        rc-update add "$name" default 2>/dev/null || true
        rc-service "$name" start
        sleep 2
        rc-service "$name" status 2>&1 | grep -qi started \
            || warn "$name OpenRC service didn't come up cleanly — check: rc-service $name status"
    else
        warn "No systemd or OpenRC on this system — falling back to a self-restarting background loop \
for $name. Not a real service; set up this system's actual init/service manager to run \
'$exec_cmd' persistently when you can."
        local supervisor="$WORK_DIR/$name-supervisor.sh"
        cat > "$supervisor" <<EOF
#!/usr/bin/env bash
$env_shell
while true; do
    $exec_cmd
    sleep 5
done
EOF
        chmod +x "$supervisor"
        nohup "$supervisor" >"$LOG_DIR/$name.log" 2>&1 &
        echo $! > "$WORK_DIR/$name.pid"
        sleep 2
        if command -v crontab &>/dev/null; then
            ( crontab -l 2>/dev/null | grep -vF "$supervisor"
              echo "@reboot $supervisor >>$LOG_DIR/$name.log 2>&1 &" ) | crontab - \
                && log "Added a cron @reboot entry, so $name also restarts after a reboot." \
                || warn "Couldn't add a cron @reboot entry — $name will NOT survive a reboot on this system."
        else
            warn "No cron either — $name will NOT survive a reboot on this system."
        fi
    fi
}

# Undoes install_supervised_service() for a given name — stops/disables/
# removes whichever tier was used, best-effort across all three since we
# don't track which one a given machine got.
remove_supervised_service() {
    local name="$1"
    command -v systemctl &>/dev/null && { systemctl disable --now "$name.service" 2>/dev/null || true; rm -f "/etc/systemd/system/$name.service"; systemctl daemon-reload 2>/dev/null || true; }
    if command -v rc-update &>/dev/null; then
        rc-service "$name" stop 2>/dev/null || true
        rc-update del "$name" default 2>/dev/null || true
        rm -f "/etc/init.d/$name"
    fi
    if command -v crontab &>/dev/null; then
        ( crontab -l 2>/dev/null | grep -vF "$WORK_DIR/$name-supervisor.sh" ) | crontab - 2>/dev/null || true
    fi
    pkill -f "$WORK_DIR/$name-supervisor.sh" 2>/dev/null || true
    rm -f "$WORK_DIR/$name-supervisor.sh"
}

ensure_container_runtime() {
    [[ "$WITH_CRI" -eq 1 ]] || return 0

    if command -v containerd &>/dev/null && command -v runc &>/dev/null; then
        log "containerd + runc already present."
    else
        pkg_install "containerd/runc" "containerd runc" "containerd runc" "containerd runc" "containerd runc" "containerd runc" "containerd runc" || true
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
        # If containerd came from a distro package, it almost certainly
        # already shipped its own containerd.service — use that (just not
        # enabled/started yet) instead of writing a generic one over it,
        # since the packaged unit is likely better-tuned (cgroup delegation,
        # OOM score, etc.) than anything worth generating here.
        if command -v systemctl &>/dev/null && systemctl list-unit-files containerd.service &>/dev/null; then
            log "containerd has an existing systemd unit — enabling and starting it..."
            systemctl enable --now containerd.service
            sleep 2
            systemctl is-active --quiet containerd.service \
                || die "containerd.service didn't come up — check: journalctl -u containerd -n 50"
        else
            # Absolute path, not the bare command: systemd/OpenRC services
            # get a fresh, minimal PATH that doesn't include wherever this
            # script's own PATH additions put a fetched/built binary
            # ($TOOLCHAIN_DIR/bin) — a bare name here resolves fine in this
            # script's own shell and then fails with "not found" (exit 127)
            # the moment the service manager actually runs it.
            install_supervised_service containerd "containerd container runtime (installed by not-k8s)" \
                "$(command -v containerd)" ""
        fi
        for _ in $(seq 1 15); do
            [[ -S /run/containerd/containerd.sock ]] && break
            sleep 1
        done
        [[ -S /run/containerd/containerd.sock ]] || die "containerd did not create its socket — check $LOG_DIR/containerd.log (or journalctl -u containerd)"
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
            "cni-plugins" "cni-plugins" "containernetworking-plugins" "containernetworking-plugins"; then
        # Distro packages install to their own dir; point ours at it. Using
        # the exact path just verified to actually have the plugin binary —
        # NOT `command -v bridge`, which found the wrong thing in testing:
        # `bridge` is also the name of an unrelated standard Linux
        # networking tool (part of iproute2, for managing bridge devices),
        # commonly present at /usr/sbin/bridge independent of whether CNI
        # plugins are installed at all. That silently pointed CNI_BIN_DIR at
        # the wrong directory, and nothing downstream (containerd's config)
        # ever got told, so RunPodSandbox couldn't find the real plugins —
        # pods stuck forever instead of erroring somewhere visible.
        local distro_cni_dir=""
        [[ -x /usr/lib/cni/bridge ]] && distro_cni_dir=/usr/lib/cni
        [[ -x /usr/libexec/cni/bridge ]] && distro_cni_dir=/usr/libexec/cni
        if [[ -n "$distro_cni_dir" ]]; then
            CNI_BIN_DIR="$distro_cni_dir"
            log "Using distro CNI plugins in $CNI_BIN_DIR"
            return 0
        fi
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
    command -v flanneld &>/dev/null || pkg_install "flannel" "flannel" "flannel" "flannel" "flannel" "flannel" "flannel" || true

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

# flanneld's kube-subnet-mgr mode still needs an explicit net-conf.json for
# every IP family, not just dual/v6 — there's no ConfigMap in this
# standalone (non-DaemonSet) setup for it to fall back to, and it has no
# built-in default Network/Backend. Writing the file now lives entirely in
# deploy/run-flanneld.sh, which regenerates it on every single service
# start (not just once, here, at install time) — see that file's header
# for why: a systemd unit's Restart=always only re-runs ExecStart, never
# this installer, so a one-time write here can't recover from the config
# going missing after a reboot. Confirmed for real: exactly that happened
# on a live test machine — /etc/kube-flannel/net-conf.json wasn't there
# after a reboot, and flanneld crash-looped forever with no way to recover
# short of manually re-running this whole script.
start_flanneld() {
    pgrep -x flanneld &>/dev/null && return 0
    # set -u makes a bare $KUBECONFIG reference itself fatal when the
    # variable was never exported (e.g. --skip-control-plane, or --with-cri
    # running before setup_control_plane gets to export it) — default it the
    # same way run_and_verify() does before testing/using it.
    local KUBECONFIG="${KUBECONFIG:-/etc/rancher/k3s/k3s.yaml}"
    [[ -f "$KUBECONFIG" ]] || die "flanneld needs a KUBECONFIG (control plane must be up first)."
    log "Starting flanneld (kube subnet manager, backend=vxlan, ip-family=$IP_FAMILY)..."
    # flanneld does NOT read the KUBECONFIG env var — its own flag-parsing
    # code (not generic client-go behavior) only looks at an explicit
    # kubeconfig flag or --master; with neither set it skips straight to
    # in-cluster config, which only works when actually running as a pod.
    # The flag is -kubeconfig-file, NOT -kubeconfig: confirmed against this
    # flannel build's actual -h usage output ("-kubeconfig-file string
    # kubeconfig file location") after --kubeconfig itself failed with
    # "flag provided but not defined: -kubeconfig" — the earlier fix was
    # built off a client-go warning message's wording ("Neither --kubeconfig
    # nor --master was specified"), which turned out not to match this
    # binary's actual registered flag name. The usage dump is the authority
    # here, not a log message's phrasing.
    # Absolute path, not the bare command: systemd/OpenRC services get a
    # fresh, minimal PATH that doesn't include wherever this script's own
    # PATH additions put a fetched/built binary ($TOOLCHAIN_DIR/bin) — a
    # bare name here resolves fine in this script's own shell and then
    # fails with "not found" (exit 127) the moment the service manager
    # actually runs it. Confirmed for real: exactly this failure, in
    # journalctl -u flanneld, on the first version of this fix.
    local flanneld_bin; flanneld_bin="$(command -v flanneld)"
    local exec_cmd="$SCRIPT_DIR/run-flanneld.sh"
    local node_name="${NODELET_NODE_NAME:-$(hostname)}"
    install_supervised_service flanneld "flanneld — CNI overlay network daemon for not-k8s" \
        "$exec_cmd" "k3s.service" \
        "NODE_NAME=$node_name" "FLANNELD_BIN=$flanneld_bin" "KUBECONFIG=$KUBECONFIG" \
        "IP_FAMILY=$IP_FAMILY" "IPV4_CLUSTER_CIDR=$IPV4_CLUSTER_CIDR" "IPV6_CLUSTER_CIDR=$IPV6_CLUSTER_CIDR"
    # Deliberately not waiting for /run/flannel/subnet.env here: flanneld's
    # kube subnet manager can't get one until a Node object exists *with an
    # allocated PodCIDR* — which needs nodelet to have registered the node
    # first, and nodelet doesn't start until run_and_verify(), later in
    # main(). Checking this early would be structurally premature on every
    # single run, not just occasionally — see wait_for_flannel_subnet(),
    # called after nodelet's own node-registration wait succeeds instead.
}

# Called from run_and_verify() only after the node has actually registered
# (so a PodCIDR has had a chance to be allocated) — this is the point where
# flanneld having no subnet.env yet is an actual problem worth a warning,
# not just normal startup ordering.
wait_for_flannel_subnet() {
    [[ "$WITH_CRI" -eq 1 && "$CNI_PLUGIN" == "flannel" ]] || return 0
    [[ -f /run/flannel/subnet.env ]] && return 0
    log "Waiting for flanneld to pick up this node's PodCIDR..."
    for _ in $(seq 1 30); do
        [[ -f /run/flannel/subnet.env ]] && { log "flannel subnet ready: $(grep FLANNEL_SUBNET /run/flannel/subnet.env 2>/dev/null)"; return 0; }
        sleep 1
    done
    warn "flanneld still hasn't written /run/flannel/subnet.env after the node registered — \
that's an actual problem now, not just startup ordering. Check $LOG_DIR/flanneld.log and \
'kubectl get node -o jsonpath={.items[0].spec.podCIDR}' (empty means the controller-manager \
hasn't allocated one — check its --allocate-node-cidrs/--cluster-cidr flags)."
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
# Service proxy (ClusterIP/NodePort) deps — nodelet itself watches Services/
# Endpoints and programs the rules (crates/nodelet/src/svc.rs); this just
# makes sure `nft` exists and that bridged pod traffic actually reaches the
# host's netfilter tables (br_netfilter), since without that a pod calling a
# ClusterIP never hits the DNAT rule at all.
# ─────────────────────────────────────────────────────────────────────────

ensure_nft() {
    [[ "$WITH_CRI" -eq 1 && "$CNI_PLUGIN" != "none" ]] || return 0
    command -v nft &>/dev/null && return 0
    pkg_install "nftables" "nftables" "nftables" "nftables" "nftables" "nftables" "nftables" || true
    command -v nft &>/dev/null \
        || warn "Could not get nftables — ClusterIP/NodePort routing will be unavailable. \
nodelet detects this and skips the Service proxy; direct pod-IP traffic is unaffected."
}

enable_bridge_netfilter() {
    [[ "$WITH_CRI" -eq 1 && "$CNI_PLUGIN" != "none" ]] || return 0
    modprobe br_netfilter 2>/dev/null || true
    sysctl -w net.ipv4.ip_forward=1 >/dev/null 2>&1 || true
    if [[ -f /proc/sys/net/bridge/bridge-nf-call-iptables ]]; then
        sysctl -w net.bridge.bridge-nf-call-iptables=1 >/dev/null 2>&1 || true
    else
        warn "net.bridge.bridge-nf-call-iptables isn't present (br_netfilter didn't load — common in \
nested/unprivileged containers) — pods calling a ClusterIP may not be DNAT'd. Traffic \
originated by the host itself still works."
    fi
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

    # Copy out to a stable path before the end-of-run cleanup wipes target/
    # (the whole cargo build cache — deps, incremental, fingerprints — none
    # of which is needed once the binary exists).
    mkdir -p "$REPO_ROOT/bin"
    install -m 0755 "$REPO_ROOT/target/release/nodelet" "$REPO_ROOT/bin/nodelet"
    log "nodelet built: $REPO_ROOT/bin/nodelet"
}

# ─────────────────────────────────────────────────────────────────────────
# Run it
# ─────────────────────────────────────────────────────────────────────────

NODELET_UNIT_SYSTEMD=/etc/systemd/system/nodelet.service
NODELET_UNIT_OPENRC=/etc/init.d/nodelet
NODELET_SUPERVISOR_SCRIPT="$WORK_DIR/nodelet-supervisor.sh"

# Installs nodelet as a real, persistent, auto-restarting service — this
# replaces an earlier version of this script that only `nohup`'d nodelet
# tied to the invoking shell, which died with the terminal, a reboot, or any
# crash, with nothing to bring it back. k3s itself already gets exactly this
# treatment (a real service); there was no reason nodelet shouldn't too.
#
# Three tiers, matching what's actually available:
#   1. systemd (Restart=always, enabled on boot) — the common case.
#   2. OpenRC (supervise-daemon, respawn, added to the default runlevel) —
#      the only other init system k3s's own installer supports, so nothing
#      running k3s at all should lack both this and systemd.
#   3. Neither: a self-restarting background loop, persisted across reboots
#      via a cron @reboot entry if cron exists. Not equivalent to a real
#      service — surfaced with a clear warning, not silently accepted as
#      good enough — but still means a crash recovers on its own instead of
#      needing a human to notice and restart it by hand.
install_nodelet_service() {
    if command -v systemctl &>/dev/null; then
        install_nodelet_service_systemd
    elif command -v rc-service &>/dev/null && command -v rc-update &>/dev/null; then
        install_nodelet_service_openrc
    else
        install_nodelet_service_fallback
    fi
}

nodelet_env_lines() { # $1 = "export VAR=value" (shell) or "Environment=VAR=value" (systemd)
    local style="$1" out=""
    for kv in "KUBECONFIG=$KUBECONFIG" "NODELET_RUNTIME=$NODELET_RUNTIME" \
              "NODELET_IP_FAMILY=$IP_FAMILY" "NODELET_LB_METHOD=$LB_METHOD"; do
        [[ "$style" == "systemd" ]] && out+="Environment=$kv"$'\n' || out+="export $kv"$'\n'
    done
    if [[ -n "${NODELET_CRI_ENDPOINT:-}" ]]; then
        [[ "$style" == "systemd" ]] && out+="Environment=NODELET_CRI_ENDPOINT=$NODELET_CRI_ENDPOINT"$'\n' \
            || out+="export NODELET_CRI_ENDPOINT=$NODELET_CRI_ENDPOINT"$'\n'
    fi
    printf '%s' "$out"
}

install_nodelet_service_systemd() {
    log "Installing nodelet as a systemd service (Restart=always, enabled on boot)..."
    cat > "$NODELET_UNIT_SYSTEMD" <<EOF
[Unit]
Description=nodelet — not-k8s node agent (kubelet replacement)
Documentation=https://github.com/centerionware/not-k8s
After=k3s.service network-online.target
Wants=k3s.service network-online.target

[Service]
Type=simple
WorkingDirectory=$REPO_ROOT
ExecStart=$SCRIPT_DIR/run-nodelet.sh
Restart=always
RestartSec=5s
$(nodelet_env_lines systemd)
[Install]
WantedBy=multi-user.target
EOF
    systemctl daemon-reload
    systemctl enable --now nodelet.service
    sleep 3
    systemctl is-active --quiet nodelet.service \
        || warn "nodelet.service didn't come up cleanly — check: journalctl -u nodelet -n 50"
}

install_nodelet_service_openrc() {
    log "Installing nodelet as an OpenRC service (supervised, auto-restart, added to boot)..."
    cat > "$NODELET_UNIT_OPENRC" <<EOF
#!/sbin/openrc-run
description="nodelet — not-k8s node agent (kubelet replacement)"

$(nodelet_env_lines shell)
supervisor="supervise-daemon"
command="$SCRIPT_DIR/run-nodelet.sh"
respawn_max=0
respawn_delay=5

depend() {
    need net
    after k3s
}
EOF
    chmod +x "$NODELET_UNIT_OPENRC"
    rc-update add nodelet default 2>/dev/null || true
    rc-service nodelet start
    sleep 3
    rc-service nodelet status 2>&1 | grep -qi started \
        || warn "nodelet OpenRC service didn't come up cleanly — check: rc-service nodelet status"
}

install_nodelet_service_fallback() {
    warn "No systemd or OpenRC on this system (k3s's own installer only supports those two, so \
this is unusual for a box running k3s) — falling back to a self-restarting background loop. \
This recovers from a crash but is NOT a real service; set up this system's actual init/service \
manager to run '$SCRIPT_DIR/run-nodelet.sh' persistently when you can."
    cat > "$NODELET_SUPERVISOR_SCRIPT" <<EOF
#!/usr/bin/env bash
$(nodelet_env_lines shell)
while true; do
    "$SCRIPT_DIR/run-nodelet.sh"
    sleep 5
done
EOF
    chmod +x "$NODELET_SUPERVISOR_SCRIPT"
    nohup "$NODELET_SUPERVISOR_SCRIPT" >"$LOG_DIR/nodelet.log" 2>&1 &
    echo $! > "$WORK_DIR/nodelet.pid"
    sleep 3

    if command -v crontab &>/dev/null; then
        ( crontab -l 2>/dev/null | grep -vF "$NODELET_SUPERVISOR_SCRIPT"
          echo "@reboot $NODELET_SUPERVISOR_SCRIPT >>$LOG_DIR/nodelet.log 2>&1 &" ) | crontab - \
            && log "Added a cron @reboot entry, so nodelet also restarts after a reboot." \
            || warn "Couldn't add a cron @reboot entry — nodelet will NOT survive a reboot on this system."
    else
        warn "No cron either — nodelet will NOT survive a reboot on this system."
    fi
}

run_and_verify() {
    export KUBECONFIG="${KUBECONFIG:-/etc/rancher/k3s/k3s.yaml}"
    if [[ ! -f "$KUBECONFIG" ]]; then
        warn "No KUBECONFIG at $KUBECONFIG — control plane wasn't set up (see above)."
        warn "nodelet needs an apiserver to register against; stopping before running it."
        return 0
    fi

    export NODELET_RUNTIME="mock"
    [[ "$WITH_CRI" -eq 1 ]] && NODELET_RUNTIME="cri"
    export NODELET_IP_FAMILY="$IP_FAMILY"
    export NODELET_LB_METHOD="$LB_METHOD"

    log "Starting nodelet (runtime=$NODELET_RUNTIME)..."
    install_nodelet_service

    log "Waiting for the node to register..."
    for i in $(seq 1 20); do
        if kubectl get nodes --no-headers 2>/dev/null | grep -q .; then
            break
        fi
        sleep 2
    done
    kubectl get nodes -o wide || warn "kubectl get nodes failed — check: journalctl -u nodelet -n 50 (or $LOG_DIR/nodelet.log if systemd isn't available)"

    wait_for_flannel_subnet

    log "Applying demo pod..."
    kubectl apply -f "$REPO_ROOT/deploy/demo-pod.yaml"
    sleep 3
    kubectl get pods -o wide

    log "Done. Logs: journalctl -u nodelet -f"
    log "Tear everything down with: $0 --cleanup"
}

# ─────────────────────────────────────────────────────────────────────────
# Footprint cleanup — everything past this point is about disk, not
# functionality. Only *build-only* tools are in scope (see the usage
# header): rustc/cargo, the C/C++ toolchain, protoc, Go, and git if this
# script installed it fresh purely to `git clone` one of the above.
# Runtime pieces the cluster keeps needing (containerd, runc, flanneld, CNI
# plugins, nftables, k3s) are never touched here.
# ─────────────────────────────────────────────────────────────────────────

# Must match the `name` argument nodelet's build-only pkg_install call sites
# use, so cleanup only ever removes packages installed for compiling things,
# never packages a *running* cluster still depends on.
BUILD_ONLY_PKG_NAMES=" C toolchain C++ compiler rust protoc go git "

remove_pkgs_via_mgr() {
    local mgr="$1" pkgs="$2"
    case "$mgr" in
        apt)    $SUDO apt-get remove -y -qq $pkgs >>"$LOG_DIR/pkg.log" 2>&1 ;;
        dnf)    $SUDO dnf remove -y -q $pkgs >>"$LOG_DIR/pkg.log" 2>&1 ;;
        yum)    $SUDO yum remove -y -q $pkgs >>"$LOG_DIR/pkg.log" 2>&1 ;;
        pacman) $SUDO pacman -Rns --noconfirm $pkgs >>"$LOG_DIR/pkg.log" 2>&1 ;;
        apk)    $SUDO apk del $pkgs >>"$LOG_DIR/pkg.log" 2>&1 ;;
        zypper) $SUDO zypper --non-interactive remove $pkgs >>"$LOG_DIR/pkg.log" 2>&1 ;;
        xbps)   $SUDO xbps-remove -Ry $pkgs >>"$LOG_DIR/pkg.log" 2>&1 ;;
    esac
}

# Removes only build-only packages (see BUILD_ONLY_PKG_NAMES) — used after
# every normal build, since the cluster still needs runtime packages
# (containerd/runc/nftables/CNI plugins/flannel) running.
uninstall_tracked_build_packages() {
    uninstall_tracked_packages_matching "$BUILD_ONLY_PKG_NAMES"
}

# Removes every package this script installed, build or runtime — used by
# --uninstall, which tears down the whole deployment, not just the build.
uninstall_all_tracked_packages() {
    uninstall_tracked_packages_matching ""
}

# $1: space-padded whitelist of logical names to remove (e.g.
# BUILD_ONLY_PKG_NAMES), or "" to remove everything logged regardless of name.
uninstall_tracked_packages_matching() {
    local whitelist="$1"
    [[ -f "$WORK_DIR/pkg_installs.log" ]] || return 0
    local removed_any=0
    while IFS='|' read -r mgr pkg_name pkgs; do
        if [[ -n "$whitelist" ]]; then
            case "$whitelist" in
                *" $pkg_name "*) ;;
                *) continue ;; # not in the whitelist — leave it installed
            esac
        fi
        [[ -z "$pkgs" ]] && continue
        log "Removing package(s) this script installed: $pkgs (via $mgr)"
        remove_pkgs_via_mgr "$mgr" "$pkgs" \
            || warn "Failed to remove '$pkgs' via $mgr — leaving it installed (see $LOG_DIR/pkg.log)."
        removed_any=1
    done < "$WORK_DIR/pkg_installs.log"
    if [[ "$removed_any" -eq 1 && "$PKG_MGR" == "apt" ]]; then
        $SUDO apt-get autoremove -y -qq >>"$LOG_DIR/pkg.log" 2>&1 || true
    fi
    rm -f "$WORK_DIR/pkg_installs.log"
}

cleanup_build_footprint() {
    [[ "$KEEP_BUILD_TOOLS" -eq 1 ]] && { log "Skipping build-toolchain cleanup (--keep-build-tools)."; return 0; }
    log "Cleaning up build toolchain (keeping the built binary + whatever the cluster needs running)..."

    # All download/build scratch — tarballs, extracted sources, git clones
    # of gcc/binutils/go/protobuf/containerd/runc/plugins/flannel/cni-plugin.
    # None of it is needed once the binaries it produced exist elsewhere.
    rm -rf "$SRC_DIR"

    # Self-contained build toolchains under .bootstrap/ — always ours,
    # always safe, regardless of whether they came from rustup, a musl.cc
    # static download, or a from-source build.
    rm -rf "$TOOLCHAIN_DIR/rustup" "$TOOLCHAIN_DIR/cargo" "$TOOLCHAIN_DIR/go" \
           "$TOOLCHAIN_DIR/gcc-src-build" "$TOOLCHAIN_DIR/protoc-dist" "$TOOLCHAIN_DIR/protoc-src-build"
    rm -rf "$TOOLCHAIN_DIR"/*-cross 2>/dev/null
    # $TOOLCHAIN_DIR/bin is mixed: build-only (cc/gcc/g++/protoc/go) sits
    # next to runtime binaries (runc/containerd/flanneld) — remove only the
    # named build-only entries, never glob the whole directory.
    rm -f "$TOOLCHAIN_DIR/bin"/{cc,gcc,g++,protoc,go}

    # cargo's build cache (deps, incremental, fingerprints) — the only
    # thing worth keeping was already copied to bin/nodelet.
    rm -rf "$REPO_ROOT/target"

    # Build-only *system* packages this run installed fresh (never anything
    # that pre-existed, and never containerd/runc/CNI/flannel/nftables).
    uninstall_tracked_build_packages

    log "Footprint cleanup done. $(du -sh "$REPO_ROOT/bin/nodelet" 2>/dev/null | cut -f1) binary at $REPO_ROOT/bin/nodelet is what's left of the build."
}

stop_running_components() {
    log "Stopping nodelet..."
    command -v systemctl &>/dev/null && systemctl stop nodelet.service 2>/dev/null
    command -v rc-service &>/dev/null && rc-service nodelet stop 2>/dev/null
    # The fallback supervisor loop's own pid doesn't cascade to whatever
    # run-nodelet.sh child it's currently mid-restart-cycle with.
    pkill -f "$SCRIPT_DIR/run-nodelet.sh" 2>/dev/null
    [[ -f "$WORK_DIR/nodelet.pid" ]] && kill "$(cat "$WORK_DIR/nodelet.pid")" 2>/dev/null || true
    log "Removing the Service-proxy nftables table (if present)..."
    command -v nft &>/dev/null && nft delete table inet not_k8s_svc 2>/dev/null || true
    log "Stopping flanneld..."
    command -v systemctl &>/dev/null && systemctl stop flanneld.service 2>/dev/null
    command -v rc-service &>/dev/null && rc-service flanneld stop 2>/dev/null
    pkill -f "flanneld --kube-subnet-mgr" 2>/dev/null
    [[ -f "$WORK_DIR/flanneld.pid" ]] && kill "$(cat "$WORK_DIR/flanneld.pid")" 2>/dev/null || true
    log "Stopping containerd (if this script started it)..."
    command -v systemctl &>/dev/null && [[ -f /etc/systemd/system/containerd.service ]] && systemctl stop containerd.service 2>/dev/null
    command -v rc-service &>/dev/null && rc-service containerd stop 2>/dev/null
    [[ -f "$WORK_DIR/containerd.pid" ]] && kill "$(cat "$WORK_DIR/containerd.pid")" 2>/dev/null || true
}

# Full teardown for --cleanup: stop everything this script started running,
# uninstall k3s and every build-only package it installed (a normal run
# already did this at the end automatically unless --keep-build-tools was
# used — this covers that case too), and remove every trace under
# $WORK_DIR/bin/target. Runtime packages (containerd/runc/nftables/CNI
# plugins/flannel) and k3s's own config/data are left in place, so the next
# run doesn't have to reinstall them — use --uninstall to remove those too.
# Undoes install_nodelet_service() — nodelet is entirely this project's own
# thing (unlike containerd/runc/CNI/flannel, which other software might
# also depend on), so it gets the same treatment as k3s itself: removed by
# --cleanup, not held back for "next run" the way shared runtime infra is.
remove_nodelet_service() {
    if command -v systemctl &>/dev/null; then
        systemctl disable --now nodelet.service 2>/dev/null || true
        rm -f "$NODELET_UNIT_SYSTEMD"
        systemctl daemon-reload 2>/dev/null || true
    fi
    if command -v rc-update &>/dev/null; then
        rc-service nodelet stop 2>/dev/null || true
        rc-update del nodelet default 2>/dev/null || true
        rm -f "$NODELET_UNIT_OPENRC"
    fi
    if command -v crontab &>/dev/null; then
        ( crontab -l 2>/dev/null | grep -vF "$NODELET_SUPERVISOR_SCRIPT" ) | crontab - 2>/dev/null || true
    fi
    rm -f "$NODELET_SUPERVISOR_SCRIPT"
}

full_cleanup() {
    stop_running_components
    remove_nodelet_service
    remove_supervised_service flanneld
    if command -v k3s-uninstall.sh &>/dev/null; then
        log "Uninstalling k3s..."
        $SUDO k3s-uninstall.sh || true
    fi
    uninstall_tracked_build_packages
    log "Removing $WORK_DIR, $REPO_ROOT/bin, and $REPO_ROOT/target..."
    rm -rf "$WORK_DIR" "$REPO_ROOT/bin" "$REPO_ROOT/target"
    log "Cleanup done. Runtime packages (containerd/runc/nftables/CNI plugins/flannel) were left installed for next time — pass --uninstall for a full teardown."
}

# --uninstall: the nuclear option. Everything --cleanup does, plus k3s's own
# data/config, containerd/runc's state and binaries, and all CNI/flannel
# config and binaries — but only for pieces this script actually installed.
# containerd/runc get the same "was it already here" check pkg tracking gives
# packages: if $TOOLCHAIN_DIR/bin/containerd exists or "containerd/runc" was
# logged as installed, this script put it there and its state is fair game;
# otherwise it predates this script (e.g. Docker's containerd) and is left
# completely alone, config/data included. CNI/flannel use the same idea, keyed
# on the "flannel" CNI plugin binary specifically — nothing else installs a
# file with that name into /opt/cni/bin, so its presence reliably means this
# script's ensure_cni() wrote the whole CNI/flannel setup being removed.
# --force's package sweep: the same logical-name -> per-manager-package-name
# mapping every pkg_install call site above already uses, duplicated here
# (not derived from pkg_installs.log) specifically *because* --force exists
# for when that log doesn't have the answer — e.g. a run from before this
# tracking existed, or from a machine this session never touched. Matched
# by name and removed unconditionally; a package that isn't installed is a
# harmless no-op for every package manager here.
force_remove_known_packages() {
    log "--force: removing every package this project could ever install, by name — not just what a tracking log says, since that log may not exist for whatever installed things here."
    local entries=(
        "C toolchain|build-essential|gcc make|base-devel|build-base|gcc make|base-devel"
        "C++ compiler|g++|gcc-c++|base-devel|g++|gcc-c++|base-devel"
        "rust|cargo rustc|cargo rustc|rust|cargo|cargo rustc|rust"
        "protoc|protobuf-compiler|protobuf-compiler|protobuf|protobuf|protobuf-devel|protobuf"
        "go|golang-go|golang|go|go|go|go"
        "containerd/runc|containerd runc|containerd runc|containerd runc|containerd runc|containerd runc|containerd runc"
        "CNI plugins|containernetworking-plugins|containernetworking-plugins|cni-plugins|cni-plugins|containernetworking-plugins|containernetworking-plugins"
        "flannel|flannel|flannel|flannel|flannel|flannel|flannel"
        "nftables|nftables|nftables|nftables|nftables|nftables|nftables"
        "git|git|git|git|git|git|git"
    )
    local col
    case "$PKG_MGR" in
        apt) col=2 ;; dnf) col=3 ;; pacman) col=4 ;; apk) col=5 ;; zypper) col=6 ;; xbps) col=7 ;;
        *) warn "Unrecognized package manager — can't force-remove system packages by name, only files/dirs/processes."; return 0 ;;
    esac
    local entry pkgs
    for entry in "${entries[@]}"; do
        pkgs="$(cut -d'|' -f"$col" <<<"$entry")"
        [[ -z "$pkgs" ]] && continue
        log "Force-removing (if present): $pkgs"
        remove_pkgs_via_mgr "$PKG_MGR" "$pkgs" || true
    done
    [[ "$PKG_MGR" == "apt" ]] && { $SUDO apt-get autoremove -y -qq >>"$LOG_DIR/pkg.log" 2>&1 || true; }
    rm -f "$WORK_DIR/pkg_installs.log"
}

full_uninstall() {
    log "Full uninstall: k3s, containerd/runc, CNI plugins, flannel, nftables — everything this script ever installed."

    local we_own_containerd=0 we_own_cni=0
    if [[ "$FORCE_UNINSTALL" -eq 1 ]]; then
        log "--force: treating containerd/runc and any CNI/flannel setup found as ours to remove, regardless of who installed them."
        we_own_containerd=1
        we_own_cni=1
    else
        { [[ -e "$TOOLCHAIN_DIR/bin/containerd" ]] || { [[ -f "$WORK_DIR/pkg_installs.log" ]] && grep -q '|containerd/runc|' "$WORK_DIR/pkg_installs.log"; }; } \
            && we_own_containerd=1
        [[ -e "$CNI_BIN_DIR/flannel" ]] && we_own_cni=1
    fi

    stop_running_components
    remove_nodelet_service
    remove_supervised_service flanneld
    # stop_running_components only kills what *this invocation's* pidfiles
    # know about. Ownership here is decided by package/binary provenance
    # instead (we_own_containerd/we_own_cni above, or unconditionally under
    # --force), which also covers a containerd/flanneld left running from an
    # earlier, unrelated (or untracked) run of this script.
    [[ "$we_own_containerd" -eq 1 ]] && { remove_supervised_service containerd; $SUDO pkill -x containerd 2>/dev/null || true; }
    [[ "$we_own_cni" -eq 1 ]] && { $SUDO pkill -x flanneld 2>/dev/null || true; }
    sleep 1

    if command -v k3s-uninstall.sh &>/dev/null; then
        log "Uninstalling k3s..."
        $SUDO k3s-uninstall.sh || true
    elif [[ "$FORCE_UNINSTALL" -eq 1 ]]; then
        log "--force: no k3s-uninstall.sh found — removing k3s manually (service, binary, symlinks)..."
        $SUDO systemctl stop k3s 2>/dev/null || true
        $SUDO systemctl disable k3s 2>/dev/null || true
        $SUDO pkill -x k3s 2>/dev/null || true
        $SUDO rm -f /etc/systemd/system/k3s.service \
            /usr/local/bin/k3s /usr/local/bin/kubectl /usr/local/bin/crictl /usr/local/bin/ctr
    fi
    log "Removing leftover k3s config/data (if any)..."
    $SUDO rm -rf /etc/rancher /var/lib/rancher

    if [[ "$we_own_containerd" -eq 1 ]]; then
        log "Removing containerd/runc state and binaries..."
        $SUDO rm -rf /etc/containerd /run/containerd /var/lib/containerd
        rm -f "$TOOLCHAIN_DIR/bin"/{containerd,containerd-shim-runc-v2,ctr,runc}
    else
        log "containerd/runc predate this script — leaving them and their state untouched."
    fi

    if [[ "$we_own_cni" -eq 1 ]]; then
        log "Removing CNI plugins, flannel, and their config..."
        $SUDO rm -rf "$CNI_BIN_DIR" "$CNI_CONF_DIR" /etc/kube-flannel /run/flannel
        rm -f "$TOOLCHAIN_DIR/bin/flanneld"
    else
        log "No CNI/flannel setup found — nothing to remove there."
    fi

    if [[ "$FORCE_UNINSTALL" -eq 1 ]]; then
        force_remove_known_packages
    else
        uninstall_all_tracked_packages
    fi
    log "Removing $WORK_DIR, $REPO_ROOT/bin, and $REPO_ROOT/target..."
    rm -rf "$WORK_DIR" "$REPO_ROOT/bin" "$REPO_ROOT/target"
    log "Full uninstall done — the system should be back to (close to) how it was before this script ran."
}

# ─────────────────────────────────────────────────────────────────────────
# Main
# ─────────────────────────────────────────────────────────────────────────

if [[ "$DO_UNINSTALL" -eq 1 ]]; then
    full_uninstall
    exit 0
fi

if [[ "$DO_CLEANUP" -eq 1 ]]; then
    full_cleanup
    exit 0
fi

# If anything from here on fails partway — a version gate that's gone stale
# again (this has happened: MIN_CARGO_MINOR did, once), a network blip
# mid-download, whatever — don't leave a half-installed build toolchain
# behind with no way back. Automatically revert whatever build-only
# packages/scratch this run added, the same cleanup a successful run does
# automatically at the end. --keep-build-tools opts out of this too, same
# as the normal end-of-run cleanup.
on_failure() {
    local code=$?
    [[ "$code" -eq 0 ]] && return
    warn "Failed (exit $code) — reverting build-only installs this run made so far..."
    cleanup_build_footprint
}
trap on_failure EXIT

log "not-k8s bootstrap-test: isolated single-script deployment"
ensure_c_toolchain
ensure_rust
setup_control_plane
build_nodelet
ensure_container_runtime
ensure_cni
ensure_nft
enable_bridge_netfilter
run_and_verify
cleanup_build_footprint
