# lib/test/cases/probes.sh — liveness/readiness/startup probes, both exec
# and httpGet forms. Exec-based tests use marker files a container creates
# itself, checked via `test -f` inside the probe (no exec/logs needed from
# us — the probe *is* the in-container check). The httpGet test exercises
# probes.rs's hand-rolled HTTP client for real, against a real pod IP.

test_readiness_probe_gates_ready_condition() {
    if ! node_uses_cri_runtime; then skip_test "needs cri runtime"; fi
    local name="readiness-gate"
    apply_manifest <<EOF
apiVersion: v1
kind: Pod
metadata:
  name: $name
spec:
  containers:
    - name: app
      image: $TEST_IMAGE
      command: ["sh", "-c", "sleep 6; touch /tmp/ready; sleep 3600"]
      readinessProbe:
        exec:
          command: ["test", "-f", "/tmp/ready"]
        periodSeconds: 2
        failureThreshold: 1
EOF
    wait_until 30 "$name Running" pod_is_phase "$name" Running
    assert_eq "$(pod_condition_status "$name" Ready)" "False" "must not be Ready before the marker file exists"
    wait_until 30 "$name Ready" bash -c "[[ \"\$(pod_condition_status '$name' Ready)\" == 'True' ]]"
    delete_pod_if_exists "$name"
}

test_liveness_probe_failure_restarts_the_container() {
    if ! node_uses_cri_runtime; then skip_test "needs cri runtime"; fi
    local name="liveness-restart"
    apply_manifest <<EOF
apiVersion: v1
kind: Pod
metadata:
  name: $name
spec:
  containers:
    - name: app
      image: $TEST_IMAGE
      command: ["sh", "-c", "touch /tmp/alive; sleep 6; rm -f /tmp/alive; sleep 3600"]
      livenessProbe:
        exec:
          command: ["test", "-f", "/tmp/alive"]
        periodSeconds: 2
        failureThreshold: 2
EOF
    wait_until 30 "$name Running" pod_is_phase "$name" Running
    # marker disappears at ~6s; 2 consecutive 2s-period failures -> restart
    # by roughly 10-12s. Generous timeout for CI/real-hardware variance.
    wait_until 60 "restart count > 0 after liveness failure" bash -c \
        "[[ \"\$(pod_container_restart_count '$name' app)\" -gt 0 ]]"
    delete_pod_if_exists "$name"
}

test_liveness_probes_own_grace_period_overrides_the_pods() {
    # Round 44: a liveness probe's own terminationGracePeriodSeconds must
    # win over the pod's own (real kubelet's documented override rule) —
    # previously restart_container() always used a hardcoded 10s
    # regardless of either. The container traps and ignores SIGTERM, so it
    # can only actually die via the grace-period SIGKILL — if the probe's
    # short override (3s) weren't honored and the pod's long one (60s) were
    # used instead, this test's own wait_until would time out and fail.
    if ! node_uses_cri_runtime; then skip_test "needs cri runtime"; fi
    local name="liveness-grace-override"
    apply_manifest <<EOF
apiVersion: v1
kind: Pod
metadata:
  name: $name
spec:
  terminationGracePeriodSeconds: 60
  containers:
    - name: app
      image: $TEST_IMAGE
      command: ["sh", "-c", "trap '' TERM; touch /tmp/alive; sleep 6; rm -f /tmp/alive; sleep 3600"]
      livenessProbe:
        exec:
          command: ["test", "-f", "/tmp/alive"]
        periodSeconds: 2
        failureThreshold: 2
        terminationGracePeriodSeconds: 3
EOF
    wait_until 30 "$name Running" pod_is_phase "$name" Running
    # marker disappears ~6s in; two consecutive 2s-period failures trip
    # liveness around t=8-10s; +3s grace = restart by roughly t=13s. 40s is
    # a generous bound that's still nowhere near the pod's 60s default —
    # timing out here is itself proof the pod's grace period leaked through
    # instead of the probe's own.
    wait_until 40 "restart count > 0 after liveness failure (probe's own grace period must be honored)" bash -c \
        "[[ \"\$(pod_container_restart_count '$name' app)\" -gt 0 ]]"
    delete_pod_if_exists "$name"
}

test_startup_probe_gates_liveness_and_readiness() {
    if ! node_uses_cri_runtime; then skip_test "needs cri runtime"; fi
    local name="startup-gate"
    apply_manifest <<EOF
apiVersion: v1
kind: Pod
metadata:
  name: $name
spec:
  containers:
    - name: app
      image: $TEST_IMAGE
      command: ["sh", "-c", "sleep 6; touch /tmp/started; sleep 3600"]
      startupProbe:
        exec:
          command: ["test", "-f", "/tmp/started"]
        periodSeconds: 2
        failureThreshold: 30
      readinessProbe:
        exec:
          command: ["test", "-f", "/tmp/started"]
        periodSeconds: 2
EOF
    wait_until 30 "$name Running" pod_is_phase "$name" Running
    assert_eq "$(pod_condition_status "$name" Ready)" "False" "readiness must not even be probed until startup passes"
    wait_until 40 "$name Ready once startup completes" bash -c \
        "[[ \"\$(pod_condition_status '$name' Ready)\" == 'True' ]]"
    delete_pod_if_exists "$name"
}

test_http_get_readiness_probe_against_a_real_server() {
    if ! node_uses_cri_runtime; then skip_test "needs cri runtime"; fi
    local name="httpget-probe"
    apply_manifest <<EOF
apiVersion: v1
kind: Pod
metadata:
  name: $name
spec:
  containers:
    - name: app
      image: $TEST_IMAGE
      command: ["sh", "-c", "mkdir -p /www && echo ok > /www/healthz && busybox httpd -f -p 8080 -h /www"]
      readinessProbe:
        httpGet:
          path: /healthz
          port: 8080
        periodSeconds: 2
        initialDelaySeconds: 2
EOF
    # This one needs real pod networking (CNI) — the probe connects to the
    # pod's real IP, not localhost. Skip cleanly if the pod never gets one
    # (e.g. --cni=none / hostNetwork-only deployments).
    wait_until 30 "$name Running" pod_is_phase "$name" Running
    local pod_ip
    pod_ip="$(pod_field "$name" '{.status.podIP}')"
    if [[ -z "$pod_ip" ]]; then
        delete_pod_if_exists "$name"
        skip_test "pod has no IP (no CNI networking configured on this node)"
    fi
    wait_until 30 "$name Ready via httpGet probe" bash -c \
        "[[ \"\$(pod_condition_status '$name' Ready)\" == 'True' ]]"
    delete_pod_if_exists "$name"
}

test_grpc_probe_manual_note() {
    skip_test "exercising grpc probes for real needs a container that actually speaks grpc.health.v1.Health/Check — TEST_IMAGE (busybox-style) doesn't, and this suite doesn't bundle a gRPC server image. Manual spot-check: deploy a pod running something that exposes the standard health-checking protocol (etcd does, out of the box, on its client port; or any grpc-health-probe-compatible workload), set readinessProbe.grpc.port to that port (and .service if the workload registers a named service rather than reporting overall health), and confirm the pod reaches Ready. Also worth checking: an unreachable/wrong port should leave the pod NOT Ready (proof check_grpc()'s timeout/connect-failure path works, not just the success path) — probes_tests/network_checks.rs already covers those failure paths against a real (non-grpc) TCP listener, so this is really just proving the success path end to end."
}

register_test test_readiness_probe_gates_ready_condition
register_test test_liveness_probe_failure_restarts_the_container
register_test test_liveness_probes_own_grace_period_overrides_the_pods
register_test test_startup_probe_gates_liveness_and_readiness
register_test test_http_get_readiness_probe_against_a_real_server
register_test test_grpc_probe_manual_note
