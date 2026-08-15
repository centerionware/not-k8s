#!/usr/bin/env bash
# bootstrap-release.sh — deploy not-k8s from a prebuilt GitHub Release
# binary instead of compiling from source on-device.
#
# This is the seam nodelet-build.sh's own doc comment already described:
# build_nodelet() checks $NOTK8S_NODELET_PREBUILT before ever touching
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
#   --layout=combined  Fetch the single multi-call `notk8s` binary (every
#                  component in one file, ~5MB smaller than the separate
#                  pair on aarch64) instead of one binary per component.
#                  Passed through to bootstrap-source.sh as well, which
#                  installs it with a bin/<component> symlink per component.
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

LAYOUT="${NOTK8S_BUILD_LAYOUT:-split}"

for arg in "$@"; do
    case "$arg" in
        --tag=*) TAG="${arg#--tag=}" ;;
        # Passed through too — bootstrap-source.sh needs it to know which
        # layout to install, this script only needs it to know what to fetch.
        --layout=*) LAYOUT="${arg#--layout=}"; PASSTHROUGH_ARGS+=("$arg") ;;
        *) PASSTHROUGH_ARGS+=("$arg") ;;
    esac
done

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

rm -f "$RELEASE_JSON"

log "Handing off to bootstrap-source.sh with NOTK8S_NODELET_PREBUILT=$NOTK8S_NODELET_PREBUILT"
exec "$SCRIPT_DIR/bootstrap-source.sh" "${PASSTHROUGH_ARGS[@]}"
