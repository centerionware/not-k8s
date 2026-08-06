#!/usr/bin/env bash
# e2e-quick-diag.sh — small, fast diagnostic snapshot printed right after
# an individual e2e test fails (wired into harness.sh's run_test(), gated
# behind NOTK8S_E2E_DEBUG_ON_FAIL=1 so normal/local runs stay quiet).
#
# Deliberately lightweight (node conditions/taints one-liner + last 15
# nodelet log lines) rather than reusing e2e-debug-dump.sh's full dump —
# that one is verbose enough that running it after every failing test in
# a suite with several failures would flood the log and make the real
# signal harder to find, not easier. This is meant to answer one
# question per failure: "was the node degraded (pressure/taint) at the
# moment this test gave up, or does nodelet's own tail show something
# test-specific?" — e2e-debug-dump.sh's full detail is still there at the
# end of the run for anything this doesn't answer.
set -uo pipefail

KUBECONFIG="${KUBECONFIG:-/etc/rancher/k3s/k3s.yaml}"
export KUBECONFIG

echo "  [diag] node conditions: $(kubectl get nodes -o jsonpath='{.items[0].status.conditions[*].type}{" "}{.items[0].status.conditions[*].status}' 2>&1)"
echo "  [diag] node taints: $(kubectl get nodes -o jsonpath='{.items[0].spec.taints}' 2>&1)"
echo "  [diag] nodelet tail:"
sudo journalctl -u nodelet.service --no-pager -n 15 2>&1 | sed 's/^/    /'
