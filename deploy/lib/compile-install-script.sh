#!/usr/bin/env bash
# compile-install-script.sh — generate the tiny standalone installer users
# actually `curl -fsSL <url> | bash` with no repo clone at all.
#
# Deliberately does NOT self-extract an embedded payload from its own
# trailing bytes (the first version of this script tried exactly that,
# tar+base64'ing deploy/ into the file itself) — bash's script-reading
# from a pipe (`bash -s` under `curl | bash`) consumes stdin in
# implementation-defined chunks while executing, which unpredictably eats
# into any trailing data the script itself tries to read afterward.
# Confirmed for real: worked perfectly when run as a real saved file,
# silently extracted nothing at all when actually piped — exactly the
# case this script has to support. Real self-extracting installers that
# support true `curl | bash` (rustup-init, Homebrew's installer) sidestep
# this by not reading their own trailing bytes either; this does the same
# — one plain curl fetch of a separate deploy.tar.gz (a real GitHub
# Release asset, so "latest" and per-version URLs come for free from
# Releases' own naming, no extra branch-publishing machinery needed for
# it), then exec bootstrap-release.sh from the extracted directory
# exactly as if it had been cloned normally.
#
# Usage: compile-install-script.sh <version> <output-path> [--pin]
#   <version>      Version string (no leading 'v') — embedded in the
#                  script's own banner; with --pin, also becomes the
#                  default --tag (and the deploy.tar.gz URL is pinned to
#                  that exact release rather than "latest").
#   <output-path>  Where to write the compiled script.
#   --pin          Used for the versioned copy that never gets
#                  overwritten; omitted for the "latest" copy.
set -euo pipefail

VERSION="${1:?usage: compile-install-script.sh <version> <output-path> [--pin]}"
OUTPUT="${2:?usage: compile-install-script.sh <version> <output-path> [--pin]}"
PIN="${3:-}"

REPO="${NOTK8S_RELEASE_REPO:-centerionware/not-k8s}"

if [[ "$PIN" == "--pin" ]]; then
    TARBALL_URL="https://github.com/$REPO/releases/download/v$VERSION/deploy.tar.gz"
    PIN_LINE='set -- "--tag=v'"$VERSION"'" "$@"'
else
    TARBALL_URL="https://github.com/$REPO/releases/latest/download/deploy.tar.gz"
    PIN_LINE=""
fi

mkdir -p "$(dirname "$OUTPUT")"

cat > "$OUTPUT" <<HEADER
#!/usr/bin/env bash
# not-k8s standalone installer — compiled by CI (deploy/lib/compile-install-script.sh)
# at release v$VERSION. Fetches deploy.tar.gz from that release's GitHub
# Release assets, extracts it to \$NOTK8S_INSTALL_DIR (default
# /opt/not-k8s), then hands off to bootstrap-release.sh (fetches a
# prebuilt nodelet binary matching this host's arch — no Rust toolchain,
# no repo clone needed for any of it). Flags after the script
# (--with-cri, --tag=vX.Y.Z, etc.) pass straight through to it.
#
# Usage:
#   curl -fsSL <this file's raw URL> | bash -s -- --with-cri
#   curl -fsSL <this file's raw URL> | bash -s -- --with-cri --layout=combined
#
# --layout=combined installs the single multi-call \`notk8s\` binary (every
# component in one executable, ~30% smaller than the separate per-component
# binaries) instead of one binary per component. Same components, same
# services, same configuration either way — see docs/ARCHITECTURE.md.
set -euo pipefail

INSTALL_DIR="\${NOTK8S_INSTALL_DIR:-/opt/not-k8s}"
mkdir -p "\$INSTALL_DIR"

TARBALL="\$(mktemp)"
trap 'rm -f "\$TARBALL"' EXIT
echo "==> Fetching $TARBALL_URL..." >&2
if command -v curl &>/dev/null; then
    curl -fsSL "$TARBALL_URL" -o "\$TARBALL"
elif command -v wget &>/dev/null; then
    wget -q -O "\$TARBALL" "$TARBALL_URL"
else
    echo "not-k8s installer: need curl or wget to fetch $TARBALL_URL" >&2
    exit 1
fi
tar xzf "\$TARBALL" -C "\$INSTALL_DIR"

$PIN_LINE
exec "\$INSTALL_DIR/deploy/bootstrap-release.sh" "\$@"
HEADER

chmod +x "$OUTPUT"
echo "compiled: $OUTPUT"
