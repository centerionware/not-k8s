# lib/test/cases/log_rotation.sh — container log rotation. The default
# threshold (10Mi) is impractically large to fill in a portable test, so
# this needs nodelet running with a small NODELET_CONTAINER_LOG_MAX_SIZE_BYTES
# for the duration of this one test — round 123: nodelet_restart_with_env
# (nodelet_env.sh) does that for real now, instead of relying on an
# externally pre-configured nodelet + a TEST_LOG_MAX_SIZE_BYTES hint.

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
    # Round 123 (found live in CI): the plain (non-sudo) `ls` here
    # returned nothing for the full 60s even though sudo-based
    # diagnostics proved app_0.log.1 (and .2/.3/.4) genuinely existed the
    # whole time — /var/log/pods and/or its contents are root-owned, and
    # this suite's own process runs as an unprivileged user (test-e2e.sh
    # itself isn't run under sudo). sudo here, matching how every other
    # host-log/host-path read in this suite already needs it.
    # Temporary diagnostics (round 123): (1) the live nodelet process's
    # own real environment, proving whether the systemd drop-in's
    # NODELET_CONTAINER_LOG_MAX_SIZE_BYTES=4096 actually reached the
    # process nodelet is running as, rather than assuming it did; (2) a
    # background poller sampling the live file's size every 10s during
    # the wait — try_wait_until swallows its condition command's own
    # stdout/stderr, so growth pattern has to be captured out-of-band
    # like this instead of printed inline.
    local nodelet_pid growth_log
    nodelet_pid="$(sudo systemctl show nodelet.service -p MainPID --value 2>/dev/null)"
    warn "[diag] nodelet.service MainPID=$nodelet_pid env: $(sudo tr '\0' '\n' < "/proc/$nodelet_pid/environ" 2>/dev/null | grep -i LOG_MAX)"
    growth_log="$(mktemp)"
    ( for _ in $(seq 1 6); do sudo bash -c "stat -c '%n %s bytes' '$log_dir'/app_0.log 2>&1" >> "$growth_log"; sleep 10; done ) &
    local growth_pid=$!

    if ! try_wait_until 60 bash -c "sudo ls '$log_dir'/app_*.log.1 >/dev/null 2>&1"; then
        kill "$growth_pid" 2>/dev/null; wait "$growth_pid" 2>/dev/null
        warn "[diag] app_0.log size over time (10s samples): $(cat "$growth_log" 2>&1)"
        warn "[diag] contents of $log_dir: $(sudo ls -la "$log_dir" 2>&1)"
        warn "[diag] nodelet log mentioning rotation:"
        sudo journalctl -u nodelet --no-pager 2>/dev/null | grep -iE "rotat" | tail -20 | while IFS= read -r line; do warn "[diag]   $line"; done
        rm -f "$growth_log"
        delete_pod_if_exists "$name"
        die "no rotated log file appeared within 60s despite NODELET_CONTAINER_LOG_MAX_SIZE_BYTES=4096 — check log rotation wiring"
    fi
    kill "$growth_pid" 2>/dev/null; wait "$growth_pid" 2>/dev/null
    rm -f "$growth_log"
    delete_pod_if_exists "$name"
}

register_test test_log_rotation_creates_a_rotated_file
