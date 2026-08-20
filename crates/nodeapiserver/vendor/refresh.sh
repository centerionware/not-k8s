#!/usr/bin/env bash
# refresh.sh — re-vendor the two upstream artifact sets nodeapiserver's
# build.rs (Group A codegen) reads at compile time:
#
#   vendor/openapi-spec/v3/*.json   — one per served group-version, carries
#     the x-kubernetes-* strategic-merge-patch/server-side-apply/discovery
#     metadata k8s-openapi's generated types don't (see docs/APISERVER_PLAN.md
#     finding 5).
#   vendor/protos/**/generated.proto — the proto2 wire schema for every
#     built-in type (finding 6); build.rs parses these into a
#     (message, jsonName) -> (field number, wire type, repeated) table rather
#     than generating a second struct universe with prost.
#
# Vendor from the real upstream tree, don't hand-reconstruct — same
# reasoning deploy/lib/e2e-full-setup.sh's own doc comment gives for the
# CSI/DRA reference drivers. Records the exact ref fetched in REF so a
# future re-run (or a human) knows what's currently vendored and can diff
# against a newer release cleanly.
#
# Usage: ./refresh.sh [git-ref]   (default: release-1.34)
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REF="${1:-release-1.34}"
REPO="kubernetes/kubernetes"

log() { echo "==> $*" >&2; }

command -v gh >/dev/null || { log "gh CLI is required"; exit 1; }
# gh's own --jq flag uses its bundled gojq, not a system jq binary.

OPENAPI_DIR="$SCRIPT_DIR/openapi-spec/v3"
PROTO_DIR="$SCRIPT_DIR/protos"
rm -rf "$OPENAPI_DIR" "$PROTO_DIR"
mkdir -p "$OPENAPI_DIR" "$PROTO_DIR"

log "listing $REPO@$REF tree..."
TREE_JSON="$(mktemp)"
trap 'rm -f "$TREE_JSON"' EXIT
gh api "repos/$REPO/git/trees/$REF?recursive=1" --paginate --jq '.tree[].path' > "$TREE_JSON"

fetch_one() {
    local path="$1" dest="$2"
    mkdir -p "$(dirname "$dest")"
    gh api "repos/$REPO/contents/$path?ref=$REF" -H 'Accept: application/vnd.github.raw' > "$dest"
}

log "fetching openapi-spec/v3/*.json..."
n=0
while IFS= read -r path; do
    [[ "$path" == api/openapi-spec/v3/*.json ]] || continue
    fetch_one "$path" "$OPENAPI_DIR/$(basename "$path")"
    n=$((n + 1))
done < "$TREE_JSON"
log "fetched $n openapi-spec files"

log "fetching **/generated.proto..."
n=0
while IFS= read -r path; do
    [[ "$path" == staging/src/k8s.io/api*/generated.proto ]] || continue
    # Strip the "staging/src/" prefix so the vendored layout mirrors the
    # real Go import path (k8s.io/api/...) that each .proto's own `package`/
    # `option go_package` lines reference — build.rs's parser resolves
    # cross-file message references by that path.
    rel="${path#staging/src/}"
    fetch_one "$path" "$PROTO_DIR/$rel"
    n=$((n + 1))
done < "$TREE_JSON"
log "fetched $n generated.proto files"

cat > "$SCRIPT_DIR/REF" <<EOF
# Upstream ref currently vendored into openapi-spec/ and protos/.
# Re-vendor with: ./refresh.sh <ref>
$REF
EOF

log "done — vendored $REPO@$REF into $SCRIPT_DIR"
