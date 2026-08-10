#!/usr/bin/env bash
# run-nodeproxy.sh — Convenience launcher for the nodeproxy binary.
#
# nodeproxy is the Service (ClusterIP/NodePort) proxy — kube-proxy's job,
# via nftables. It's a separate process from nodelet on purpose (see
# crates/nodeproxy/src/main.rs); neither needs the other running.
#
# Mirrors run-nodelet.sh: picks up environment variables the user has set,
# fills in sane defaults, prints what it's about to do, and execs the binary
# (replacing this shell process so signals propagate cleanly).
#
# Usage:
#   ./deploy/run-nodeproxy.sh
#   NODEPROXY_IP_FAMILY=dual NODEPROXY_LB_METHOD=source-hash ./deploy/run-nodeproxy.sh
#
set -euo pipefail

# ── Locate the binary ───────────────────────────────────────────────────────

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

# Same resolution order as run-nodelet.sh: bootstrap-source.sh copies the
# built binary to bin/ and then wipes target/ entirely, so that's the first
# place to look; target/release/ is still checked for a plain `cargo build`
# dev workflow that never went through that cleanup.
if [[ -n "${NODEPROXY_BIN:-}" ]]; then
    : # explicit override, use as-is
elif [[ -x "${REPO_ROOT}/bin/nodeproxy" ]]; then
    NODEPROXY_BIN="${REPO_ROOT}/bin/nodeproxy"
elif [[ -x "${REPO_ROOT}/bin/notk8s" ]]; then
    # Combined layout (--layout=combined) normally installs a bin/nodeproxy
    # symlink to bin/notk8s, which the branch above picks up like any other
    # binary. This is the fallback for when only the combined binary itself
    # survived — a copy that didn't preserve symlinks, a hand-dropped
    # single-file install — where naming the component explicitly is the
    # same dispatch by another route.
    NODEPROXY_BIN="${REPO_ROOT}/bin/notk8s"
    set -- nodeproxy "$@"
else
    NODEPROXY_BIN="${REPO_ROOT}/target/release/nodeproxy"
fi

if [[ ! -x "$NODEPROXY_BIN" ]]; then
    echo "ERROR: nodeproxy binary not found or not executable at: $NODEPROXY_BIN" >&2
    echo "" >&2
    echo "Build it first:" >&2
    echo "  cd $REPO_ROOT" >&2
    echo "  cargo build --release -p nodeproxy" >&2
    exit 1
fi

# ── Defaults ─────────────────────────────────────────────────────────────────

export KUBECONFIG="${KUBECONFIG:-/etc/rancher/k3s/k3s.yaml}"
export RUST_LOG="${RUST_LOG:-info}"

# ── Startup banner ───────────────────────────────────────────────────────────

echo "========================================"
echo " not-k8s nodeproxy (Service routing)"
echo "========================================"
echo "  Binary:      $NODEPROXY_BIN"
echo "  KUBECONFIG:  $KUBECONFIG"
echo "  RUST_LOG:    $RUST_LOG"

# NODELET_* are the pre-split spellings; nodeproxy still accepts them, so
# print whichever is actually set rather than pretending only one exists.
[[ -n "${NODEPROXY_IP_FAMILY:-${NODELET_IP_FAMILY:-}}" ]] \
    && echo "  IP family:   ${NODEPROXY_IP_FAMILY:-${NODELET_IP_FAMILY:-}}"
[[ -n "${NODEPROXY_LB_METHOD:-${NODELET_LB_METHOD:-}}" ]] \
    && echo "  LB method:   ${NODEPROXY_LB_METHOD:-${NODELET_LB_METHOD:-}}"

echo "========================================"
echo ""

# ── Preflight ────────────────────────────────────────────────────────────────

if [[ ! -f "$KUBECONFIG" ]]; then
    echo "WARNING: Kubeconfig not found at $KUBECONFIG" >&2
    echo "         Is the k3s control plane running?  See deploy/setup-control-plane.sh" >&2
    echo "" >&2
fi

if ! command -v nft &>/dev/null; then
    echo "WARNING: nft (nftables) not found on PATH — nodeproxy needs it (and" >&2
    echo "         CAP_NET_ADMIN/root) to program ClusterIP/NodePort rules, the" >&2
    echo "         same requirement real kube-proxy has. It will exit non-zero." >&2
    echo "" >&2
fi

# ── Exec ─────────────────────────────────────────────────────────────────────

exec "$NODEPROXY_BIN" "$@"
