#!/usr/bin/env bash
# run-nodelet.sh — Convenience launcher for the nodelet binary.
#
# Picks up environment variables the user has set, fills in sane defaults,
# prints what it's about to do, and execs the binary (replacing this shell
# process so signals propagate cleanly).
#
# Usage:
#   ./deploy/run-nodelet.sh                          # defaults (mock runtime)
#   NODELET_RUNTIME=cri ./deploy/run-nodelet.sh      # real containerd
#   NODELET_NODE_NAME=edge-42 ./deploy/run-nodelet.sh
#
set -euo pipefail

# ── Locate the binary ───────────────────────────────────────────────────────

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

NODELET_BIN="${NODELET_BIN:-${REPO_ROOT}/target/release/nodelet}"

if [[ ! -x "$NODELET_BIN" ]]; then
    echo "ERROR: nodelet binary not found or not executable at: $NODELET_BIN" >&2
    echo "" >&2
    echo "Build it first:" >&2
    echo "  cd $REPO_ROOT" >&2
    echo "  cargo build --release                    # mock runtime" >&2
    echo "  cargo build --release --features cri     # CRI/containerd runtime" >&2
    exit 1
fi

# ── Defaults ─────────────────────────────────────────────────────────────────

# Kubeconfig: default to k3s's standard path; override with KUBECONFIG env var.
export KUBECONFIG="${KUBECONFIG:-/etc/rancher/k3s/k3s.yaml}"

# Logging: default to info-level; override with RUST_LOG env var.
export RUST_LOG="${RUST_LOG:-info}"

# Node name: let nodelet default to hostname if unset.
# We read it here just for the startup banner.
NODE_NAME="${NODELET_NODE_NAME:-$(hostname)}"
RUNTIME="${NODELET_RUNTIME:-mock}"

# ── Startup banner ───────────────────────────────────────────────────────────

echo "========================================"
echo " not-k8s nodelet"
echo "========================================"
echo "  Binary:      $NODELET_BIN"
echo "  Node name:   $NODE_NAME"
echo "  Runtime:     $RUNTIME"
echo "  KUBECONFIG:  $KUBECONFIG"
echo "  RUST_LOG:    $RUST_LOG"

# Print optional overrides if they are set.
[[ -n "${NODELET_CRI_ENDPOINT:-}" ]]   && echo "  CRI endpoint:  $NODELET_CRI_ENDPOINT"
[[ -n "${NODELET_HEARTBEAT_SECS:-}" ]] && echo "  Heartbeat:     ${NODELET_HEARTBEAT_SECS}s"
[[ -n "${NODELET_STATUS_SECS:-}" ]]    && echo "  Status push:   ${NODELET_STATUS_SECS}s"
[[ -n "${NODELET_CPU:-}" ]]            && echo "  CPU capacity:  $NODELET_CPU cores"
[[ -n "${NODELET_MEMORY_BYTES:-}" ]]   && echo "  Memory cap:    $NODELET_MEMORY_BYTES bytes"
[[ -n "${NODELET_MAX_PODS:-}" ]]       && echo "  Max pods:      $NODELET_MAX_PODS"
[[ -n "${NODELET_LABELS:-}" ]]         && echo "  Extra labels:  $NODELET_LABELS"

echo "========================================"
echo ""

# ── Preflight ────────────────────────────────────────────────────────────────

if [[ ! -f "$KUBECONFIG" ]]; then
    echo "WARNING: Kubeconfig not found at $KUBECONFIG" >&2
    echo "         Is the k3s control plane running?  See deploy/setup-control-plane.sh" >&2
    echo "" >&2
fi

# ── Exec ─────────────────────────────────────────────────────────────────────
# exec replaces this shell, so:
#   - PID 1 signals (SIGTERM from systemd/docker) go straight to nodelet
#   - No zombie shell process hanging around

exec "$NODELET_BIN" "$@"
