# lib/test/cases/unimplemented.sh — documents known gaps as *active*
# checks rather than silent absence of coverage: each of these asserts that
# the feature still doesn't work. The point is a loud, specific FAILURE the
# moment someone partially implements one of these without updating this
# suite — "kubectl exec now returns 0, update this test" is a much better
# signal than a stale doc nobody noticed was wrong. See docs/GAP_CLOSURE.md
# for the real tracking of this work; delete the relevant test here (or flip
# its assertion) once the feature actually lands.

test_kubectl_exec_still_unsupported() {
    local name="exec-unsupported-check"
    apply_manifest <<EOF
apiVersion: v1
kind: Pod
metadata:
  name: $name
spec:
  containers:
    - name: app
      image: $TEST_IMAGE
      command: ["sleep", "3600"]
EOF
    wait_until 30 "$name Running" pod_is_phase "$name" Running
    if kctl exec "$name" -- true >/dev/null 2>&1; then
        delete_pod_if_exists "$name"
        die "kubectl exec SUCCEEDED — the streaming server must have been implemented since docs/GAP_CLOSURE.md was last updated. Update that doc and this test (flip this assertion / move it out of unimplemented.sh) instead of leaving this as an expected failure."
    fi
    echo "    (confirmed: kubectl exec still fails, as expected — no streaming server yet)"
    delete_pod_if_exists "$name"
}

test_kubectl_logs_still_unsupported() {
    local name="logs-unsupported-check"
    apply_manifest <<EOF
apiVersion: v1
kind: Pod
metadata:
  name: $name
spec:
  containers:
    - name: app
      image: $TEST_IMAGE
      command: ["sleep", "3600"]
EOF
    wait_until 30 "$name Running" pod_is_phase "$name" Running
    if kctl logs "$name" >/dev/null 2>&1; then
        delete_pod_if_exists "$name"
        die "kubectl logs SUCCEEDED — the streaming server must have been implemented since docs/GAP_CLOSURE.md was last updated. Update that doc and this test instead of leaving this as an expected failure."
    fi
    echo "    (confirmed: kubectl logs still fails, as expected — no streaming server yet)"
    delete_pod_if_exists "$name"
}

test_stats_summary_endpoint_manual_check() {
    skip_test "no kubelet-style HTTP(S) server exists on this node at all yet (see docs/GAP_CLOSURE.md) — there's no port to even attempt curling /stats/summary against. Once the streaming server lands, replace this with a real check against https://<node-ip>:10250/stats/summary."
}

register_test test_kubectl_exec_still_unsupported
register_test test_kubectl_logs_still_unsupported
register_test test_stats_summary_endpoint_manual_check
