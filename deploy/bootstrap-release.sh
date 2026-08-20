#!/usr/bin/env bash
# bootstrap-release.sh — deploy not-k8s from a prebuilt GitHub Release
# binary instead of compiling from source on-device.
#
# This is the seam nodelet-build.sh's own doc comment already described:
# build_nodelet() checks $NOTK8S_COMBINED_PREBUILT before ever touching
# cargo/rustc, and just installs that binary instead. This script's whole
# job is populating that variable from a real release asset, then handing
# off to bootstrap-source.sh for everything else (k3s control plane,
# containerd/runc, CNI, systemd units) exactly as it already does for a
# from-source install — no toolchain (Rust/protoc/etc.) ever gets
# installed on this device, since build_nodelet() returns before any of
# that code path runs.
#
# Usage:
#   ./deploy/bootstrap-release.sh [--with-cri] [--tag=vX.Y.Z] [any other
#       bootstrap-source.sh flag — passed through verbatim]
#
#   --tag=vX.Y.Z   Install a specific release instead of the latest one.
#   --layout=combined  Fetch the single multi-call `notk8s` binary (the
#                  default release path: every component in one file,
#                  ~5MB smaller than the separate pair on aarch64).
#                  Passed through to bootstrap-source.sh as well, which
#                  installs it with a bin/<component> symlink per component.
#   --layout=split     Opt into fetching one release binary per component.
#
# Release assets are expected to be named
# <binary>-<version>-linux-<arch>-<profile>, where <binary> is nodelet,
# nodeproxy (the Service proxy — kube-proxy's job, its own binary since the
# split), or notk8s (the combined multi-call build of both), <arch> is one of
# x86_64/aarch64/armv7l and <profile> is release or debug (matching
# .github/workflows/release.yml's own publish-release job) — release
# (optimized) is what this script fetches; there's no flag to ask for the
# debug build here, since a debug binary isn't something you'd want
# actually deployed to a real device.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
LIB_DIR="$SCRIPT_DIR/lib"

REPO="${NOTK8S_RELEASE_REPO:-centerionware/not-k8s}"
TAG=""
PASSTHROUGH_ARGS=()

# A release install runs the complete not-k8s stack from the combined asset.
# Source builds retain their split default for component-level iteration;
# this entry point is the optimized, no-toolchain release path.
LAYOUT="${NOTK8S_BUILD_LAYOUT:-combined}"

EXPLICIT_LAYOUT=false
SKIP_NODELET=false
for arg in "$@"; do
    case "$arg" in
        --tag=*) TAG="${arg#--tag=}" ;;
        # Passed through too — bootstrap-source.sh needs it to know which
        # layout to install, this script only needs it to know what to fetch.
        --layout=*) LAYOUT="${arg#--layout=}"; EXPLICIT_LAYOUT=true; PASSTHROUGH_ARGS+=("$arg") ;;
        --skip-nodelet) SKIP_NODELET=true; PASSTHROUGH_ARGS+=("$arg") ;;
        *) PASSTHROUGH_ARGS+=("$arg") ;;
    esac
done

# The combined release contains every applet. Enable every replacement by
# default too, so bootstrap-source.sh links and starts all of them and
# disables the corresponding bundled k3s control-plane processes. Explicit
# command-line flags still override these environment defaults downstream.
#
# Round 124 (found live: "ERROR: nodestore binary not found or not
# executable"): skipped entirely when --skip-nodelet is set. That flag means
# the opposite intent — a baseline comparison against k3s's own bundled
# kine/kube-scheduler/kube-controller-manager with *none* of our
# replacements (profiling.yml's upstream-kubelet comparison leg is exactly
# this) — so defaulting DATASTORE/SCHEDULER/CONTROLLER_MANAGER to "on" here
# would tell bootstrap-source.sh to run nodestore/nodescheduler/
# nodecontroller anyway, none of which this script's own --skip-nodelet
# early-exit below ever fetches (or builds — there's no toolchain on this
# path either), so they'd be missing binaries at service-start time.
if ! $SKIP_NODELET; then
    export DATASTORE="${DATASTORE:-nodestore}"
    export SCHEDULER="${SCHEDULER:-nodescheduler}"
    export CONTROLLER_MANAGER="${CONTROLLER_MANAGER:-nodecontroller}"
fi

# Round 124 (found live: every one of the three exec paths below failed with
# "NOTK8S_COMBINED_PREBUILT is set... but this run's layout is 'split'" for
# any caller that didn't pass --layout= explicitly — i.e. every real
# `install.sh`/`install-vX.Y.Z.sh` invocation, the actual published
# installers). LAYOUT above already resolved to this script's own default
# ("combined") when the caller didn't ask for one, and that's what drove the
# asset fetch — but PASSTHROUGH_ARGS only ever gained a --layout= entry when
# the caller supplied one, so bootstrap-source.sh (execed below with this
# process's env, --layout=* absent) fell back to *its own* default
# ("split") instead, and immediately died on the mismatch. Appending the
# already-resolved LAYOUT here, once, covers every exec site below with an
# explicit flag instead of relying on an implicit default neither script
# actually shares.
$EXPLICIT_LAYOUT || PASSTHROUGH_ARGS+=("--layout=$LAYOUT")

mkdir -p "$REPO_ROOT/.bootstrap/logs"
source "$LIB_DIR/common.sh"

# Validated here, not just downstream in bootstrap-source.sh: this script
# fetches release assets *before* handing off, and only branches on
# "combined". Without this, `--layout=comined` quietly downloads the
# per-component assets and only fails afterwards, once the network cost is
# already paid. Placed after common.sh on purpose — die() comes from there.
case "$LAYOUT" in
    split|combined|both) ;;
    *) die "Unknown --layout='$LAYOUT' (want 'split' — one binary per component, 'combined' — one multi-call binary, or 'both'). See deploy/lib/components.sh." ;;
esac
detect_platform   # sets ARCH
ensure_fetch_tool

# Round 124: --skip-nodelet means no nodelet binary is wanted on this host
# at all (profiling.yml's upstream-kubelet.sh comparison leg — a real
# kubelet, not nodelet, is going in) -- skip the whole release-lookup/
# download dance (including the arch-support gate below, which only
# exists because a release asset needs to actually match) and hand off
# to bootstrap-source.sh directly. It sees --skip-nodelet itself and
# skips build_nodelet()/run_and_verify() the same way, so this is the one
# code path that needs zero changes downstream — this script is just not
# in the way at all when nodelet isn't wanted.
for arg in "${PASSTHROUGH_ARGS[@]}"; do
    if [[ "$arg" == "--skip-nodelet" ]]; then
        log "Skipping the release binary fetch (--skip-nodelet) — handing off straight to bootstrap-source.sh."
        exec "$SCRIPT_DIR/bootstrap-source.sh" "${PASSTHROUGH_ARGS[@]}"
    fi
done

case "$ARCH" in
    x86_64|aarch64|armv7l) ;;
    *) die "No prebuilt release for arch '$ARCH' — .github/workflows/release.yml only builds x86_64/aarch64/armv7l. Use ./deploy/bootstrap-source.sh to build from source on this device instead." ;;
esac

if [[ -z "$TAG" ]]; then
    log "Looking up the latest release for $REPO..."
    RELEASE_JSON="$(mktemp)"
    fetch "https://api.github.com/repos/$REPO/releases/latest" "$RELEASE_JSON"
    TAG="$(grep -m1 '"tag_name"' "$RELEASE_JSON" | sed -e 's/.*: *"//' -e 's/".*//')"
    [[ -n "$TAG" ]] || die "Couldn't determine the latest release tag from the GitHub API response — is $REPO's Releases page empty? Pass --tag=vX.Y.Z to target a specific release."
else
    RELEASE_JSON="$(mktemp)"
    fetch "https://api.github.com/repos/$REPO/releases/tags/$TAG" "$RELEASE_JSON"
fi
log "Using release $TAG"

VERSION="${TAG#v}"

# download_release_binary <binary-name> — fetches
# <name>-<version>-linux-<arch>-release from this release into .bootstrap/
# and echoes the local path.
download_release_binary() {
    local name="$1" asset url path
    asset="${name}-${VERSION}-linux-${ARCH}-release"
    url="$(grep -o "\"browser_download_url\": *\"[^\"]*${asset}\"" "$RELEASE_JSON" | sed -e 's/.*"\(https[^"]*\)"/\1/')"
    [[ -n "$url" ]] || die "Release $TAG has no asset named '$asset' — check https://github.com/$REPO/releases/tag/$TAG for what's actually attached."
    log "Downloading $asset..."
    path="$REPO_ROOT/.bootstrap/$asset"
    mkdir -p "$REPO_ROOT/.bootstrap"
    fetch "$url" "$path"
    chmod +x "$path"
    echo "$path"
}

# The combined layout is one asset containing every component, so it's a
# single download and none of the per-component logic below applies —
# including --proxy=none, which in this layout means "don't run nodeproxy",
# not "don't fetch it" (there's nothing separate to skip fetching).
if [[ "$LAYOUT" == "combined" ]]; then
    # Assign, check, then export — never `export VAR="$(cmd)"`. That form
    # returns *export's* status, so `set -e` never sees the command fail,
    # and download_release_binary's own `die` only exits the subshell it
    # runs in: the net effect is an empty variable and a run that carries
    # on as if nothing were wrong.
    combined_prebuilt="$(download_release_binary notk8s)" \
        || die "Couldn't download the combined 'notk8s' binary for this release."
    export NOTK8S_COMBINED_PREBUILT="$combined_prebuilt"
    rm -f "$RELEASE_JSON"
    log "Handing off to bootstrap-source.sh with NOTK8S_COMBINED_PREBUILT=$NOTK8S_COMBINED_PREBUILT"
    exec "$SCRIPT_DIR/bootstrap-source.sh" "${PASSTHROUGH_ARGS[@]}"
fi

# Same assign-check-export as the combined branch above, for the same
# reason.
nodelet_prebuilt="$(download_release_binary nodelet)" \
    || die "Couldn't download the 'nodelet' binary for this release."
export NOTK8S_NODELET_PREBUILT="$nodelet_prebuilt"

# --proxy=none means this node's ClusterIP/NodePort routing belongs to
# something else (a real kube-proxy, Cilium) — don't fetch a binary that
# will never run. Anything else gets nodeproxy, matching the default.
WANT_PROXY=1
for arg in "${PASSTHROUGH_ARGS[@]}"; do
    [[ "$arg" == "--proxy=none" ]] && WANT_PROXY=0
done
if [[ "$WANT_PROXY" -eq 1 ]]; then
    nodeproxy_prebuilt="$(download_release_binary nodeproxy)" \
        || die "Couldn't download the 'nodeproxy' binary for this release. Pass --proxy=none if this node's service routing is handled by something else."
    export NOTK8S_NODEPROXY_PREBUILT="$nodeproxy_prebuilt"
fi

# The mirror image of --proxy=none for the control-plane datastore: the
# default release path enables nodestore, and an explicit split-layout
# install needs its matching prebuilt asset too. The combined path returned
# above before this block already contains nodestore in the one binary.
WANT_DATASTORE=0
[[ "${DATASTORE:-none}" == "nodestore" ]] && WANT_DATASTORE=1
for arg in "${PASSTHROUGH_ARGS[@]}"; do
    [[ "$arg" == "--datastore=nodestore" ]] && WANT_DATASTORE=1
    [[ "$arg" == "--datastore=none" ]] && WANT_DATASTORE=0
done
if [[ "$WANT_DATASTORE" -eq 1 ]]; then
    nodestore_prebuilt="$(download_release_binary nodestore)" \
        || die "Couldn't download the 'nodestore' binary for this release. Drop --datastore=nodestore to leave storage to k3s's bundled kine."
    export NOTK8S_NODESTORE_PREBUILT="$nodestore_prebuilt"
fi

# The mirror image of --proxy=none: nodescheduler is opt-IN, so it is fetched
# only when asked for. It has to be fetched when asked for, though — the split
# layout refuses to mix prebuilt and from-source components (see
# use_prebuilt_binaries in lib/nodelet-build.sh), so a run that enables the
# scheduler without a binary for it dies rather than quietly building one.
#
# Both spellings are checked, because bootstrap-source.sh accepts both: the
# flag and a pre-set SCHEDULER in the environment are the same setting, and
# honouring only the flag here would turn the env-var form into that same
# confusing mixing error.
WANT_SCHEDULER=0
[[ "${SCHEDULER:-none}" == "nodescheduler" ]] && WANT_SCHEDULER=1
for arg in "${PASSTHROUGH_ARGS[@]}"; do
    [[ "$arg" == "--scheduler=nodescheduler" ]] && WANT_SCHEDULER=1
    [[ "$arg" == "--scheduler=none" ]] && WANT_SCHEDULER=0
done
if [[ "$WANT_SCHEDULER" -eq 1 ]]; then
    nodescheduler_prebuilt="$(download_release_binary nodescheduler)" \
        || die "Couldn't download the 'nodescheduler' binary for this release. Drop --scheduler=nodescheduler to leave pod placement to the kube-scheduler k3s runs itself."
    export NOTK8S_NODESCHEDULER_PREBUILT="$nodescheduler_prebuilt"
fi

# Same opt-IN shape as nodescheduler above, for the same reason.
WANT_CONTROLLER_MANAGER=0
[[ "${CONTROLLER_MANAGER:-none}" == "nodecontroller" ]] && WANT_CONTROLLER_MANAGER=1
for arg in "${PASSTHROUGH_ARGS[@]}"; do
    [[ "$arg" == "--controller-manager=nodecontroller" ]] && WANT_CONTROLLER_MANAGER=1
    [[ "$arg" == "--controller-manager=none" ]] && WANT_CONTROLLER_MANAGER=0
done
if [[ "$WANT_CONTROLLER_MANAGER" -eq 1 ]]; then
    nodecontroller_prebuilt="$(download_release_binary nodecontroller)" \
        || die "Couldn't download the 'nodecontroller' binary for this release. Drop --controller-manager=nodecontroller to leave node lifecycle/podCIDR/GC to the kube-controller-manager k3s runs itself."
    export NOTK8S_NODECONTROLLER_PREBUILT="$nodecontroller_prebuilt"
fi

rm -f "$RELEASE_JSON"

log "Handing off to bootstrap-source.sh with NOTK8S_NODELET_PREBUILT=$NOTK8S_NODELET_PREBUILT"
exec "$SCRIPT_DIR/bootstrap-source.sh" "${PASSTHROUGH_ARGS[@]}"
