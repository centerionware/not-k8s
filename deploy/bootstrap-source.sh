#!/usr/bin/env bash
# bootstrap-source.sh — self-contained, build-from-source deployment for
# not-k8s on *any* Linux box: any distro, any package manager (or none),
# any CPU architecture. (A future bootstrap-binary-latest.sh will do the
# same deployment from prebuilt release binaries instead of compiling
# on-device — this script's lib/ modules are already split so that entry
# point can reuse everything except toolchain-*.sh and nodelet-build.sh's
# cargo-build path; see deploy/lib/nodelet-build.sh's header for the seam.)
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
# Prerequisites: bash and curl. Every script here is bash, not POSIX sh, so
# this cannot bootstrap a system that lacks bash — the shebang fails before
# any of the dependency tiers above get a chance to run. That is fine almost
# everywhere, but a stock Alpine image has neither:
#
#   apk add bash curl        # Alpine: required before running this at all
#
# (Alpine cloud images may also come up with no /etc/resolv.conf at all, in
# which case apk itself can't resolve its mirrors — check that first.)
#
# Usage:
#   ./deploy/bootstrap-source.sh                  # mock runtime, k3s control plane, demo pod
#   ./deploy/bootstrap-source.sh --with-cri       # also build+use the real containerd/CRI runtime
#   ./deploy/bootstrap-source.sh --with-cri --cni=none   # real containers, hostNetwork-only (old behavior)
#   ./deploy/bootstrap-source.sh --with-cri --ip-family=ipv4     # force v4-only
#   ./deploy/bootstrap-source.sh --with-cri --lb-method=round-robin
#   ./deploy/bootstrap-source.sh --with-cri --proxy=none   # no Service proxy: something else (a real kube-proxy, Cilium, ...) owns ClusterIP/NodePort routing on this node
#   ./deploy/bootstrap-source.sh --with-cri --scheduler=nodescheduler   # our scheduler instead of the kube-scheduler k3s runs in-process
#   ./deploy/bootstrap-source.sh --with-cri --controller-manager=nodecontroller   # our controller manager instead of the kube-controller-manager k3s runs in-process
#   ./deploy/bootstrap-source.sh --skip-control-plane
#   ./deploy/bootstrap-source.sh --with-cri --skip-nodelet   # control plane + containerd/CNI only, nodelet never built/installed/started (round 124: profiling.yml's upstream-kubelet.sh comparison leg wants this exact stack with a different node agent, not nodelet sitting there unused)
#   ./deploy/bootstrap-source.sh --with-cri --layout=combined  # one multi-call binary (bin/notk8s) instead of one per component
#   ./deploy/bootstrap-source.sh --with-cri --layout=both      # build both layouts; run the separate binaries
#   ./deploy/bootstrap-source.sh --keep-build-tools   # skip the end-of-run toolchain cleanup (faster re-runs)
#   ./deploy/bootstrap-source.sh --cleanup        # stop the deployment + build-tool cleanup (keeps runtime pkgs/k3s for next time)
#   ./deploy/bootstrap-source.sh --uninstall      # full teardown: also k3s, containerd/runc, CNI/flannel, nftables
#   ./deploy/bootstrap-source.sh --uninstall --force  # same, but by name — ignores tracking entirely
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
# same result for k3s's --cluster-cidr/--service-cidr, flannel, and the
# nodeproxy Service proxy, so all three agree. --lb-method: random (default; matches
# how kube-proxy iptables mode behaves) | round-robin | source-hash (sticky
# per client IP — also used automatically for any Service that sets
# `sessionAffinity: ClientIP`, regardless of this default). Both apply to
# nodeproxy, the separate Service-routing binary — kube-proxy's job, which
# nodelet used to do in-process and no longer does.
#
# --layout: split (default) | combined | both. Purely a packaging choice —
# what gets built, not how it behaves. `split` produces one binary per
# component (bin/nodelet, bin/nodeproxy): install only what this node runs,
# upgrade one without touching the other. `combined` produces a single
# multi-call binary (bin/notk8s) with a bin/<component> symlink per
# component, which it dispatches on via argv[0] — the components still run
# as separate processes and separate services, but they share one copy of
# the dependency tree they have in common (tokio/kube/k8s-openapi/rustls)
# instead of each carrying its own, which on aarch64 is ~12MB total against
# ~17MB split. `both` builds both and runs the separate binaries, leaving
# the combined one at bin/notk8s. Equivalent env var: NOTK8S_BUILD_LAYOUT.
# See deploy/lib/components.sh.
#
# --datastore: none (default) | nodestore. `none` leaves the control plane's
# storage to k3s's own bundled kine, which is what every release so far has
# done. `nodestore` additionally BUILDS the datastore binary
# (crates/nodestore — the etcd v3 API over sqlite, event-driven instead of
# kine's polling); it does not yet install it as a service or repoint the
# control plane at it, because that is an ordering constraint on the control
# plane's own startup rather than a node-side concern. Today this flag is
# what the datastore's e2e coverage uses to get a binary to test.
#
# --scheduler: none (default) | nodescheduler. `none` leaves pod placement to
# the kube-scheduler k3s already runs in-process, which is what every release
# so far has done. `nodescheduler` builds and installs crates/nodescheduler as
# its own service AND passes --disable-scheduler to k3s, because those are two
# halves of one switch: two schedulers both watching unbound pods and both
# writing Bindings is a race, not a redundant pair. Unlike --datastore, this
# needs no ordering change — a scheduler is an ordinary apiserver client, so
# its unit simply orders After=k3s.service like nodelet's and nodeproxy's do.
#
# --proxy: nodeproxy (default) | none. `none` installs no Service proxy and
# touches no nftables rules, leaving ClusterIP/NodePort routing to whatever
# else this node runs (a real kube-proxy, Cilium, kube-router). This is the
# point of nodeproxy being its own binary; nodelet is unaffected either way.
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
# The `nodeproxy` binary does it with nftables (crates/nodeproxy/src/svc.rs,
# watches Services+EndpointSlices, rebuilds one table atomically per event —
# no periodic resync). This script installs it as its own service, and makes
# sure `nft` is present and that bridged pod traffic reaches the host's
# netfilter tables (br_netfilter) so the DNAT rules actually see it. All of
# that is skipped by --proxy=none.
#
set -euo pipefail

# ─────────────────────────────────────────────────────────────────────────
# Config & flags
# ─────────────────────────────────────────────────────────────────────────

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
LIB_DIR="$SCRIPT_DIR/lib"

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
SKIP_NODELET=0
PROXY=nodeproxy
DATASTORE="${DATASTORE:-none}"
SCHEDULER="${SCHEDULER:-none}"
CONTROLLER_MANAGER="${CONTROLLER_MANAGER:-none}"
BUILD_LAYOUT="${NOTK8S_BUILD_LAYOUT:-split}"

for arg in "$@"; do
    case "$arg" in
        --with-cri) WITH_CRI=1 ;;
        --skip-control-plane) SKIP_CONTROL_PLANE=1 ;;
        --skip-nodelet) SKIP_NODELET=1 ;;
        --cleanup) DO_CLEANUP=1 ;;
        --uninstall) DO_UNINSTALL=1 ;;
        --force) FORCE_UNINSTALL=1 ;;
        --force-source-build) FORCE_SOURCE_BUILD=1 ;;
        --cni=*) CNI_PLUGIN="${arg#--cni=}" ;;
        --ip-family=*) IP_FAMILY="${arg#--ip-family=}" ;;
        --lb-method=*) LB_METHOD="${arg#--lb-method=}" ;;
        --proxy=*) PROXY="${arg#--proxy=}" ;;
        --datastore=*) DATASTORE="${arg#--datastore=}" ;;
        --scheduler=*) SCHEDULER="${arg#--scheduler=}" ;;
        --controller-manager=*) CONTROLLER_MANAGER="${arg#--controller-manager=}" ;;
        --layout=*) BUILD_LAYOUT="${arg#--layout=}" ;;
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

# lib/common.sh defines log()/warn()/die() — needed by everything below,
# including the --force/--uninstall guard right after this.
source "$LIB_DIR/common.sh"

[[ "$FORCE_UNINSTALL" -eq 1 && "$DO_UNINSTALL" -ne 1 ]] \
    && die "--force only means something alongside --uninstall (it makes --uninstall remove everything by name, not just what this script tracked installing)."

detect_platform   # sets OS, ARCH_RAW, ARCH, PKG_MGR, IS_ROOT, SUDO

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

resolve_ip_family   # sets IP_FAMILY (resolving "auto"), CLUSTER_CIDR, SERVICE_CIDR

case "$LB_METHOD" in
    random|round-robin|source-hash) ;;
    *) die "Unknown --lb-method='$LB_METHOD' (want 'random', 'round-robin', or 'source-hash')." ;;
esac

case "$PROXY" in
    nodeproxy|none) ;;
    *) die "Unknown --proxy='$PROXY' (want 'nodeproxy' or 'none')." ;;
esac

case "$DATASTORE" in
    none|nodestore) ;;
    *) die "Unknown --datastore='$DATASTORE' (want 'none' — k3s's own bundled kine, the default — or 'nodestore')." ;;
esac
# Read by lib/components.sh's want_nodestore predicate.
export DATASTORE

case "$SCHEDULER" in
    none|nodescheduler) ;;
    *) die "Unknown --scheduler='$SCHEDULER' (want 'none' — k3s's own bundled kube-scheduler, the default — or 'nodescheduler')." ;;
esac
# Read by lib/components.sh's want_nodescheduler predicate *and* by
# setup-control-plane.sh, which turns k3s's own scheduler off when ours is on.
export SCHEDULER

case "$CONTROLLER_MANAGER" in
    none|nodecontroller) ;;
    *) die "Unknown --controller-manager='$CONTROLLER_MANAGER' (want 'none' — k3s's own bundled kube-controller-manager, the default — or 'nodecontroller')." ;;
esac
# Read by lib/components.sh's want_nodecontroller predicate *and* by
# setup-control-plane.sh, which turns k3s's own controller manager off when
# ours is on.
export CONTROLLER_MANAGER

case "$BUILD_LAYOUT" in
    split|combined|both) ;;
    *) die "Unknown --layout='$BUILD_LAYOUT' (want 'split' — one binary per component, 'combined' — one multi-call binary, or 'both'). See deploy/lib/components.sh." ;;
esac
# The build layout is consumed by lib/components.sh through this env var, so
# the flag and NOTK8S_BUILD_LAYOUT are the same setting either way round.
# Validated here rather than only in resolve_build_layout() so a typo fails
# before the control plane install, not after it.
export NOTK8S_BUILD_LAYOUT="$BUILD_LAYOUT"

ensure_fetch_tool
export PATH="$TOOLCHAIN_DIR/bin:$PATH"

# ─────────────────────────────────────────────────────────────────────────
# Load every function module. Order doesn't matter — nothing below actually
# runs until the Main dispatch at the bottom calls it — except that each
# file may reference globals set above (WORK_DIR, TOOLCHAIN_DIR, ARCH, ...).
# ─────────────────────────────────────────────────────────────────────────

source "$LIB_DIR/toolchain-c.sh"
source "$LIB_DIR/toolchain-rust.sh"
source "$LIB_DIR/toolchain-go.sh"
source "$LIB_DIR/toolchain-protoc.sh"
source "$LIB_DIR/control-plane.sh"
source "$LIB_DIR/service-mgr.sh"
source "$LIB_DIR/container-runtime.sh"
source "$LIB_DIR/cni.sh"
source "$LIB_DIR/nft.sh"
source "$LIB_DIR/components.sh"
source "$LIB_DIR/nodelet-build.sh"
source "$LIB_DIR/nodelet-service.sh"
source "$LIB_DIR/nodeproxy-service.sh"
source "$LIB_DIR/nodestore-service.sh"
source "$LIB_DIR/nodescheduler-service.sh"
source "$LIB_DIR/nodecontroller-service.sh"
source "$LIB_DIR/run.sh"
source "$LIB_DIR/cleanup.sh"
source "$LIB_DIR/uninstall.sh"

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

log "not-k8s bootstrap-source: isolated single-command source deployment"
# Round 124: build_nodelet() already no-ops entirely when every enabled
# component has its own prebuilt binary supplied (nodelet-build.sh's own
# use_prebuilt_binaries(), asked ahead of time here via
# any_enabled_component_needs_build() so a toolchain isn't installed and
# then never used) — matters for real: this is exactly the case CI's own
# e2e stage hits on every shard now that it downloads a prebuilt debug
# binary from build-and-test instead of rebuilding from source.
#
# Deliberately NOT gated on SKIP_NODELET any more: want_nodelet() itself
# now excludes nodelet from the enabled-component set when SKIP_NODELET
# is set (see its own doc comment for the real bug this closes — a
# --skip-nodelet run that also wants nodestore used to install
# nodestore's own systemd service without ever building its binary,
# since the toolchain-install step here skipped installing anything to
# build it with). A run that wants nothing at all still calls this
# cheaply — any_enabled_component_needs_build() just says no.
if any_enabled_component_needs_build; then
    ensure_c_toolchain
    ensure_rust
fi
# The datastore has to exist, and be listening, *before* the control plane is
# installed — setup_control_plane hands k3s a --datastore-endpoint pointing at
# it, and k3s cannot serve at all if nothing answers there. That inverts this
# script's usual order (build the binaries after the control plane is up), so
# it's done here rather than by moving build_nodelet unconditionally: the
# default --datastore=none path keeps exactly the ordering it has always had,
# and only a run that asked for nodestore pays for the change.
#
# build_nodelet builds every *enabled* component in one pass (see
# lib/components.sh), so this is also what produces bin/nodelet and
# bin/nodeproxy on this path — hence the flag, so it isn't run twice.
# Safe to call unconditionally now (not gated on SKIP_NODELET) — nodelet
# itself is already excluded via want_nodelet() when skipped, so this
# only ever builds whatever else this run actually wants.
NODELET_ALREADY_BUILT=0
if want_nodestore; then
    build_nodelet
    NODELET_ALREADY_BUILT=1
    install_nodestore_service
    # Ordering the units is not enough on its own: systemd calls a Type=simple
    # service "started" the moment it forks, so k3s would race a store that
    # hasn't opened its socket yet. Block until it actually answers.
    wait_for_nodestore
    # Read by setup-control-plane.sh, which turns these into
    # --datastore-endpoint and the three --datastore-*file flags.
    #
    # https, and with a client certificate: the datastore has no plaintext
    # mode. It holds every Secret in the cluster and the etcd v3 API has no
    # authentication of its own, so serving it in the clear would be worse
    # than an open apiserver — there is no authorization layer above it to
    # fail closed. nodestore generates this material on first start when an
    # operator has not supplied their own; these are the paths it writes.
    #
    # Not the listen address verbatim: a wildcard bind (0.0.0.0 / [::]) is not
    # an address k3s can dial against a certificate — no SAN names a wildcard,
    # so the handshake fails with a certificate error that reads as a PKI
    # problem rather than a configuration one. nodestore_dialable_host() maps
    # it to the matching loopback, which tls_sans() always covers.
    nodestore_listen="${NODESTORE_LISTEN:-$NODESTORE_LISTEN_DEFAULT}"
    export NOTK8S_DATASTORE_ENDPOINT="https://$(nodestore_dialable_host "$nodestore_listen"):$(nodestore_listen_port "$nodestore_listen")"
    nodestore_pki="${NODESTORE_DATA_DIR:-/var/lib/nodestore}/pki/client"
    export NOTK8S_DATASTORE_CAFILE="$nodestore_pki/ca.crt"
    export NOTK8S_DATASTORE_CERTFILE="$nodestore_pki/client.crt"
    export NOTK8S_DATASTORE_KEYFILE="$nodestore_pki/client.key"
    for f in "$NOTK8S_DATASTORE_CAFILE" "$NOTK8S_DATASTORE_CERTFILE" "$NOTK8S_DATASTORE_KEYFILE"; do
        [[ -s "$f" ]] || die "nodestore is listening but did not write $f. The control plane cannot authenticate to it without that file — check: journalctl -u nodestore -n 50"
    done
    log "Control plane will use nodestore at $NOTK8S_DATASTORE_ENDPOINT over mutual TLS (instead of k3s's bundled kine)."
fi

setup_control_plane
# Not gated on SKIP_NODELET any more, same reasoning as the earlier call
# site above — want_nodelet() already excludes nodelet from what this
# builds when skipped, so this only needs to avoid running twice
# (NODELET_ALREADY_BUILT) when the nodestore branch above already did.
if [[ "$NODELET_ALREADY_BUILT" -eq 0 ]]; then
    build_nodelet
fi
ensure_container_runtime
ensure_cni
ensure_nft
enable_bridge_netfilter
if [[ "$SKIP_NODELET" -eq 0 ]]; then
    run_and_verify
else
    log "Skipping nodelet build/install/start (--skip-nodelet) — control plane + containerd + CNI are up, nothing else touches this node."
fi
cleanup_build_footprint
