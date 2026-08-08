#!/usr/bin/env bash
# not-k8s standalone installer — compiled by CI (deploy/lib/compile-install-script.sh)
# at release v0.1.1. Fetches deploy.tar.gz from that release's GitHub
# Release assets, extracts it to $NOTK8S_INSTALL_DIR (default
# /opt/not-k8s), then hands off to bootstrap-release.sh (fetches a
# prebuilt nodelet binary matching this host's arch — no Rust toolchain,
# no repo clone needed for any of it). Flags after the script
# (--with-cri, --tag=vX.Y.Z, etc.) pass straight through to it.
#
# Usage:
#   curl -fsSL <this file's raw URL> | bash -s -- --with-cri
set -euo pipefail

INSTALL_DIR="${NOTK8S_INSTALL_DIR:-/opt/not-k8s}"
mkdir -p "$INSTALL_DIR"

TARBALL="$(mktemp)"
trap 'rm -f "$TARBALL"' EXIT
echo "==> Fetching https://github.com/centerionware/not-k8s/releases/download/v0.1.1/deploy.tar.gz..." >&2
if command -v curl &>/dev/null; then
    curl -fsSL "https://github.com/centerionware/not-k8s/releases/download/v0.1.1/deploy.tar.gz" -o "$TARBALL"
elif command -v wget &>/dev/null; then
    wget -q -O "$TARBALL" "https://github.com/centerionware/not-k8s/releases/download/v0.1.1/deploy.tar.gz"
else
    echo "not-k8s installer: need curl or wget to fetch https://github.com/centerionware/not-k8s/releases/download/v0.1.1/deploy.tar.gz" >&2
    exit 1
fi
tar xzf "$TARBALL" -C "$INSTALL_DIR"

set -- "--tag=v0.1.1" "$@"
exec "$INSTALL_DIR/deploy/bootstrap-release.sh" "$@"
