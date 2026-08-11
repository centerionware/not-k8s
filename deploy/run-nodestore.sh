#!/usr/bin/env bash
# run-nodestore.sh — Convenience launcher for the nodestore binary.
#
# nodestore is the datastore: the etcd v3 gRPC API over sqlite, which the
# control plane's apiserver talks to instead of kine (see crates/nodestore's
# lib.rs). Unlike nodelet and nodeproxy it is NOT a client of the apiserver —
# it is what the apiserver stores into — so it takes no KUBECONFIG and has
# no dependency on the control plane being up. The ordering runs the other
# way: k3s cannot start until this is listening.
#
# Mirrors run-nodelet.sh / run-nodeproxy.sh: picks up environment variables
# the user has set, fills in sane defaults, prints what it's about to do, and
# execs the binary (replacing this shell so signals propagate cleanly).
#
# Usage:
#   ./deploy/run-nodestore.sh
#   NODESTORE_LISTEN=127.0.0.1:2379 ./deploy/run-nodestore.sh
#
# Every other knob is read by the binary itself from NODESTORE_* (see
# crates/nodestore/src/config.rs) — this script deliberately doesn't
# enumerate them, so a new one doesn't need a change here to be usable.
set -euo pipefail

# ── Locate the binary ───────────────────────────────────────────────────────

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

# Same resolution order as run-nodelet.sh/run-nodeproxy.sh: bootstrap-source.sh
# copies the built binary to bin/ and then wipes target/ entirely, so that's
# the first place to look; target/release/ is still checked for a plain
# `cargo build` dev workflow that never went through that cleanup.
if [[ -n "${NODESTORE_BIN:-}" ]]; then
    : # explicit override, use as-is
elif [[ -x "${REPO_ROOT}/bin/nodestore" ]]; then
    NODESTORE_BIN="${REPO_ROOT}/bin/nodestore"
elif [[ -x "${REPO_ROOT}/bin/notk8s" ]]; then
    # Combined layout: bin/nodestore is normally a symlink to bin/notk8s and
    # the branch above catches it. This is the fallback for when only the
    # combined binary survived (a copy that didn't preserve symlinks, a
    # hand-dropped single-file install) — naming the component explicitly is
    # the same argv[0] dispatch by another route.
    NODESTORE_BIN="${REPO_ROOT}/bin/notk8s"
    set -- nodestore "$@"
else
    NODESTORE_BIN="${REPO_ROOT}/target/release/nodestore"
fi

if [[ ! -x "$NODESTORE_BIN" ]]; then
    echo "ERROR: nodestore binary not found or not executable at: $NODESTORE_BIN" >&2
    echo "" >&2
    echo "Build it first:" >&2
    echo "  cd $REPO_ROOT" >&2
    echo "  cargo build --release -p nodestore" >&2
    exit 1
fi

# ── Defaults ─────────────────────────────────────────────────────────────────

export RUST_LOG="${RUST_LOG:-info}"
# Mirrors crates/nodestore/src/config.rs's own defaults. Set here too so the
# banner below tells the truth about where the data lives even when the
# caller set nothing.
export NODESTORE_LISTEN="${NODESTORE_LISTEN:-127.0.0.1:2379}"
export NODESTORE_DATA_DIR="${NODESTORE_DATA_DIR:-/var/lib/nodestore}"

mkdir -p "$NODESTORE_DATA_DIR"

# ── Startup banner ───────────────────────────────────────────────────────────

echo "========================================"
echo " not-k8s nodestore (datastore)"
echo "========================================"
echo "  Binary:      $NODESTORE_BIN"
echo "  Listen:      $NODESTORE_LISTEN"
echo "  Data dir:    $NODESTORE_DATA_DIR"
echo "  RUST_LOG:    $RUST_LOG"

# Clustered vs single-member is the single most important thing to be able to
# read back out of a log after the fact, so say which one this is.
if [[ -n "${NODESTORE_INITIAL_CLUSTER:-${NODESTORE_PEERS:-}}" ]]; then
    echo "  Mode:        replicated (raft)"
    echo "  Member ID:   ${NODESTORE_MEMBER_ID:-<unset — the binary will reject this>}"
    echo "  Cluster:     ${NODESTORE_INITIAL_CLUSTER:-${NODESTORE_PEERS:-}}"
    [[ -n "${NODESTORE_PEER_LISTEN:-}" ]] && echo "  Peer listen: $NODESTORE_PEER_LISTEN"
else
    echo "  Mode:        single member (no replication configured)"
fi

echo "========================================"
echo ""

# ── Exec ─────────────────────────────────────────────────────────────────────

exec "$NODESTORE_BIN" "$@"
