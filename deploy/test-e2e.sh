#!/usr/bin/env bash
# test-e2e.sh — end-to-end functional tests against a real, already-running
# not-k8s cluster (a stripped k3s control plane + nodelet with the real CRI
# runtime). Complements `cargo test`: the Rust suite proves pure logic
# (decision functions, parsers, translation tables) in isolation; this
# proves the whole thing actually works against a live apiserver + real
# containerd containers — the two are not substitutes for each other.
#
# This does NOT set anything up for you. Run deploy/bootstrap-source.sh
# --with-cri (or your own setup-control-plane.sh + run-nodelet.sh
# NODELET_RUNTIME=cri) first, and export KUBECONFIG so kubectl reaches it.
#
# Must run ON THE SAME NODE as nodelet: several checks (resource limits,
# securityContext, hostAliases, DNS config, log rotation) read files
# straight off nodelet's on-disk state (materialized volumes, container
# logs) because `kubectl exec`/`kubectl logs` aren't implemented yet — see
# lib/test/cases/unimplemented.sh, which documents that gap as its own
# explicit (skipped) test rather than silently having no coverage for it.
#
# Usage:
#   ./deploy/test-e2e.sh                    # run everything
#   ./deploy/test-e2e.sh --only=probes      # only tests whose function name contains "probes"
#   ./deploy/test-e2e.sh --only=probes,dns  # comma-separated: contains "probes" OR "dns" (round 123)
#   ./deploy/test-e2e.sh --keep             # don't delete the test namespace at the end (debugging)
#   ./deploy/test-e2e.sh --namespace=foo    # use a specific namespace instead of a generated one
#
# Exit code is nonzero if anything failed (not on skips) — safe to use in CI
# once a real cluster is available in that environment.
set -uo pipefail  # deliberately not -e: the harness runs each test in its
                   # own subshell and must survive individual test failures

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
LIB_DIR="$SCRIPT_DIR/lib"
TEST_LIB_DIR="$LIB_DIR/test"

ONLY_PATTERN=""
KEEP_NAMESPACE=0
TEST_NAMESPACE="not-k8s-e2e-$(date +%s)"

for arg in "$@"; do
    case "$arg" in
        --only=*) ONLY_PATTERN="${arg#--only=}" ;;
        --keep) KEEP_NAMESPACE=1 ;;
        --namespace=*) TEST_NAMESPACE="${arg#--namespace=}" ;;
        -h|--help)
            grep '^#' "$0" | sed -e 's/^# \{0,1\}//' -e '1,2d'
            exit 0
            ;;
        *) echo "Unknown flag: $arg" >&2; exit 1 ;;
    esac
done

# Several case files' wait_until/try_wait_until calls run their check via
# `bash -c "..."` — a real separate bash process, not just a subshell —
# and reference TEST_NAMESPACE inside it (directly, or transitively through
# an exported helper like kctl). Without this it's simply unset there,
# same failure mode lib/test/k8s.sh's own export -f comment describes.
export TEST_NAMESPACE

# shellcheck source=lib/common.sh
source "$LIB_DIR/common.sh"

command -v kubectl >/dev/null 2>&1 || die "kubectl not found on PATH."
kubectl get nodes >/dev/null 2>&1 || die "kubectl can't reach a cluster (check KUBECONFIG). This suite needs a live not-k8s deployment — see deploy/bootstrap-source.sh --with-cri."

# shellcheck source=lib/test/harness.sh
source "$TEST_LIB_DIR/harness.sh"
# shellcheck source=lib/test/k8s.sh
source "$TEST_LIB_DIR/k8s.sh"
# shellcheck source=lib/test/manifests.sh
source "$TEST_LIB_DIR/manifests.sh"

if ! node_uses_cri_runtime; then
    warn "Node is running the mock runtime (or its status hasn't been checked yet) — most of these tests need real containers and will SKIP. Run nodelet with NODELET_RUNTIME=cri."
fi

log "Test namespace: $TEST_NAMESPACE"
kubectl create namespace "$TEST_NAMESPACE" >/dev/null 2>&1 || true

cleanup_namespace() {
    if [[ "$KEEP_NAMESPACE" -eq 1 ]]; then
        warn "--keep set: leaving namespace $TEST_NAMESPACE in place for inspection. Clean up with: kubectl delete namespace $TEST_NAMESPACE"
        return
    fi
    log "Cleaning up test namespace $TEST_NAMESPACE..."
    kubectl delete namespace "$TEST_NAMESPACE" --wait=false >/dev/null 2>&1 || true
}
trap cleanup_namespace EXIT

# Load every case file (each one calls register_test for its test_* functions).
for case_file in "$TEST_LIB_DIR"/cases/*.sh; do
    # shellcheck source=/dev/null
    source "$case_file"
done

log "Running ${#TESTS_REGISTERED[@]} registered test(s)..."
run_all_registered_tests
print_summary

[[ "$TESTS_FAILED" -eq 0 ]]
