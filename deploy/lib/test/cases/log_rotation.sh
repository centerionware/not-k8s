# lib/test/cases/log_rotation.sh — container log rotation. The default
# threshold (10Mi) is impractically large to fill in a portable test, so
# this needs nodelet running with a small NODELET_CONTAINER_LOG_MAX_SIZE_BYTES
# for the duration of this one test — round 123: nodelet_restart_with_env
# (nodelet_env.sh) does that for real now, instead of relying on an
# externally pre-configured nodelet + a TEST_LOG_MAX_SIZE_BYTES hint.
#
# Round 123's own investigation into this test is worth recording: a
# hand-quoted `try_wait_until 60 bash -c "sudo ls '$log_dir'/app_*.log.1
# ..."` reliably reported failure for the full 60s run after run, even
# though sudo-based post-mortem diagnostics AND a direct `crictl inspect`
# of the container's own real ContainerStatus.log_path (confirming CRI
# returns it already joined with the sandbox's log_directory, exactly as
# rotate_logs() expects) both proved real rotation was happening well
# within that same window — rotation itself was never broken. The actual
# bug was in the *check*, not the feature: nesting a hand-built
# single-quoted path inside a double-quoted `bash -c "..."` string is
# exactly the kind of construct that's fragile to get right blind, even
# when it happens to parse correctly stand-alone. Using an exported
# function (the same pattern k8s.sh's own wait_until helpers already
# establish for this exact reason — see its own comment) instead of a
# hand-quoted command string sidesteps the whole class of bug.

_log_rotation_check() { # _log_rotation_check <log_dir>
    sudo ls "$1"/app_*.log.1 >/dev/null 2>&1
}
export -f _log_rotation_check

test_log_rotation_creates_a_rotated_file() {
    if ! node_uses_cri_runtime; then skip_test "needs cri runtime"; fi
    if ! nodelet_restart_supported; then skip_test "needs systemd to restart nodelet with a small NODELET_CONTAINER_LOG_MAX_SIZE_BYTES"; fi

    log_rotation_test_cleanup() { nodelet_restore_env; }
    trap log_rotation_test_cleanup EXIT
    nodelet_restart_with_env "NODELET_CONTAINER_LOG_MAX_SIZE_BYTES=4096"

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
      # A tiny per-line sleep still clears the 4096-byte threshold in
      # well under a second, but keeps the unrotated file's real size
      # sane (round 123, found live in CI: an unthrottled tight loop
      # generated 100+MB per 10s rotation tick — harmless to correctness,
      # but needlessly heavy I/O on a shared CI runner for a test that
      # only needs to prove rotation happens at all).
      command: ["sh", "-c", "while true; do echo 'filler line to grow the log file quickly'; sleep 0.01; done"]
EOF
    wait_until 60 "$name Running" pod_is_phase "$name" Running

    local ns uid log_dir
    ns="$(pod_field "$name" '{.metadata.namespace}')"
    uid="$(pod_field "$name" '{.metadata.uid}')"
    log_dir="/var/log/pods/${ns}_${name}_${uid}"

    log "    waiting for a rotated log file under $log_dir (log_rotate_interval, default 10s)..."
    if ! try_wait_until 60 _log_rotation_check "$log_dir"; then
        warn "[diag] contents of $log_dir: $(sudo ls -la "$log_dir" 2>&1)"
        warn "[diag] nodelet log mentioning rotation:"
        sudo journalctl -u nodelet --no-pager 2>/dev/null | grep -iE "rotat" | tail -20 | while IFS= read -r line; do warn "[diag]   $line"; done
        delete_pod_if_exists "$name"
        die "no rotated log file appeared within 60s despite NODELET_CONTAINER_LOG_MAX_SIZE_BYTES=4096 — check log rotation wiring"
    fi
    delete_pod_if_exists "$name"
}

register_test test_log_rotation_creates_a_rotated_file
