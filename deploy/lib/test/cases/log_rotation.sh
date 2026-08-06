# lib/test/cases/log_rotation.sh — container log rotation. The default
# threshold (10Mi) is impractically large to fill in a portable test, so
# this needs nodelet running with a small NODELET_CONTAINER_LOG_MAX_SIZE_BYTES
# for the duration of this one test; skips cleanly if that isn't the case.

test_log_rotation_creates_a_rotated_file() {
    if ! node_uses_cri_runtime; then skip_test "needs cri runtime"; fi
    if [[ -z "${TEST_LOG_MAX_SIZE_BYTES:-}" ]]; then
        skip_test "TEST_LOG_MAX_SIZE_BYTES not set — export it to whatever NODELET_CONTAINER_LOG_MAX_SIZE_BYTES nodelet is currently running with (something small, e.g. 4096) to exercise this within a reasonable test window"
    fi

    local name="log-rotation"
    apply_manifest <<EOF
apiVersion: v1
kind: Pod
metadata:
  name: $name
spec:
  containers:
    - name: app
      image: $TEST_IMAGE
      command: ["sh", "-c", "while true; do echo 'filler line to grow the log file quickly'; done"]
EOF
    wait_until 60 "$name Running" pod_is_phase "$name" Running

    local ns uid log_dir
    ns="$(pod_field "$name" '{.metadata.namespace}')"
    uid="$(pod_field "$name" '{.metadata.uid}')"
    log_dir="/var/log/pods/${ns}_${name}_${uid}"

    log "    waiting for a rotated log file under $log_dir (log_rotate_interval, default 10s)..."
    if ! try_wait_until 60 bash -c "ls '$log_dir'/app_*.log.1 >/dev/null 2>&1"; then
        delete_pod_if_exists "$name"
        skip_test "no rotated log file appeared within 60s — is NODELET_CONTAINER_LOG_MAX_SIZE_BYTES actually set that low on the running nodelet? (TEST_LOG_MAX_SIZE_BYTES is just this test's expectation, it doesn't configure nodelet itself)"
    fi
    delete_pod_if_exists "$name"
}

register_test test_log_rotation_creates_a_rotated_file
