#!/usr/bin/env bash
# run-nodecontroller.sh — Convenience launcher for the nodecontroller binary.
#
# nodecontroller is the controller-manager component — kube-controller-
# manager's job. It is a pure apiserver client: no root, no nftables, no
# container runtime, just a kubeconfig. It runs as its own process on
# purpose (see crates/nodecontroller/src/main.rs); nothing else needs it
# running, and it needs nothing else running but the apiserver.
#
# Mirrors run-nodeproxy.sh: picks up environment variables the user has set,
# fills in sane defaults, prints what it's about to do, and execs the binary
# (replacing this shell process so signals propagate cleanly).
#
# Deliberately does not enumerate every NODECONTROLLER_* knob — the crate's
# own config.rs is the list, and duplicating it here just creates a second
# copy to forget to update. The banner prints the few that change what a
# reader would see in the log.
#
# Usage:
#   ./deploy/run-nodecontroller.sh
#   NODECONTROLLER_CLUSTER_CIDR=10.42.0.0/16 ./deploy/run-nodecontroller.sh
#
set -euo pipefail

# ── Locate the binary ───────────────────────────────────────────────────────

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

# Same resolution order as run-nodeproxy.sh: bootstrap-source.sh copies the
# built binary to bin/ and then wipes target/ entirely, so that's the first
# place to look; target/release/ is still checked for a plain `cargo build`
# dev workflow that never went through that cleanup.
if [[ -n "${NODECONTROLLER_BIN:-}" ]]; then
    : # explicit override, use as-is
elif [[ -x "${REPO_ROOT}/bin/nodecontroller" ]]; then
    NODECONTROLLER_BIN="${REPO_ROOT}/bin/nodecontroller"
elif [[ -x "${REPO_ROOT}/bin/notk8s" ]]; then
    # Combined layout (--layout=combined) normally installs a
    # bin/nodecontroller symlink to bin/notk8s, which the branch above picks
    # up like any other binary. This is the fallback for when only the
    # combined binary itself survived — a copy that didn't preserve
    # symlinks, a hand-dropped single-file install — where naming the
    # component explicitly is the same dispatch by another route.
    NODECONTROLLER_BIN="${REPO_ROOT}/bin/notk8s"
    set -- nodecontroller "$@"
else
    NODECONTROLLER_BIN="${REPO_ROOT}/target/release/nodecontroller"
fi

if [[ ! -x "$NODECONTROLLER_BIN" ]]; then
    echo "ERROR: nodecontroller binary not found or not executable at: $NODECONTROLLER_BIN" >&2
    echo "" >&2
    echo "Build it first:" >&2
    echo "  cd $REPO_ROOT" >&2
    echo "  cargo build --release -p nodecontroller" >&2
    exit 1
fi

# ── Defaults ─────────────────────────────────────────────────────────────────

export KUBECONFIG="${KUBECONFIG:-/etc/rancher/k3s/k3s.yaml}"
export RUST_LOG="${RUST_LOG:-info}"

# ── Startup banner ───────────────────────────────────────────────────────────

echo "========================================"
echo " not-k8s nodecontroller (controller manager)"
echo "========================================"
echo "  Binary:      $NODECONTROLLER_BIN"
echo "  KUBECONFIG:  $KUBECONFIG"
echo "  RUST_LOG:    $RUST_LOG"

[[ -n "${NODECONTROLLER_CLUSTER_CIDR:-}" ]] \
    && echo "  Cluster CIDR: ${NODECONTROLLER_CLUSTER_CIDR}"
[[ -n "${NODECONTROLLER_LEADER_ELECT:-}" ]] \
    && echo "  Leader elect: ${NODECONTROLLER_LEADER_ELECT}"

echo "========================================"
echo ""

# ── Preflight ────────────────────────────────────────────────────────────────

if [[ ! -f "$KUBECONFIG" ]]; then
    echo "WARNING: Kubeconfig not found at $KUBECONFIG" >&2
    echo "         Is the k3s control plane running?  See deploy/setup-control-plane.sh" >&2
    echo "" >&2
fi

# ── Exec ─────────────────────────────────────────────────────────────────────

exec "$NODECONTROLLER_BIN" "$@"
