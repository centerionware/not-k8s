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
#
# Release assets are expected to be named
# nodelet-<version>-linux-<arch>-<profile>, where <arch> is one of
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

for arg in "$@"; do
    case "$arg" in
        --tag=*) TAG="${arg#--tag=}" ;;
        *) PASSTHROUGH_ARGS+=("$arg") ;;
    esac
done

mkdir -p "$REPO_ROOT/.bootstrap/logs"
source "$LIB_DIR/common.sh"
detect_platform   # sets ARCH
ensure_fetch_tool

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
ASSET_NAME="nodelet-${VERSION}-linux-${ARCH}-release"
DOWNLOAD_URL="$(grep -o "\"browser_download_url\": *\"[^\"]*${ASSET_NAME}\"" "$RELEASE_JSON" | sed -e 's/.*"\(https[^"]*\)"/\1/')"
rm -f "$RELEASE_JSON"

[[ -n "$DOWNLOAD_URL" ]] || die "Release $TAG has no asset named '$ASSET_NAME' — check https://github.com/$REPO/releases/tag/$TAG for what's actually attached."

log "Downloading $ASSET_NAME..."
PREBUILT_PATH="$REPO_ROOT/.bootstrap/$ASSET_NAME"
mkdir -p "$REPO_ROOT/.bootstrap"
fetch "$DOWNLOAD_URL" "$PREBUILT_PATH"
chmod +x "$PREBUILT_PATH"

log "Handing off to bootstrap-source.sh with NOTK8S_NODELET_PREBUILT=$PREBUILT_PATH"
export NOTK8S_NODELET_PREBUILT="$PREBUILT_PATH"
exec "$SCRIPT_DIR/bootstrap-source.sh" "${PASSTHROUGH_ARGS[@]}"
