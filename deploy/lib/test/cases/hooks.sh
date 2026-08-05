# lib/test/cases/hooks.sh — postStart/preStop lifecycle hooks and
# terminationGracePeriodSeconds. Hooks run *inside* the container, so
# there's no way to observe them via kubectl alone; each writes its proof
# into a shared emptyDir volume, read back off the host (see manifests.sh).

test_poststart_hook_runs_after_container_start() {
    if ! node_uses_cri_runtime; then skip_test "needs cri runtime"; fi
    local name="poststart-hook"
    apply_manifest <<EOF
apiVersion: v1
kind: Pod
metadata:
  name: $name
spec:
  volumes:
    - name: shared
      emptyDir: {}
  containers:
    - name: app
      image: $TEST_IMAGE
      command: ["sleep", "3600"]
      volumeMounts:
        - name: shared
          mountPath: /shared
      lifecycle:
        postStart:
          exec:
            command: ["sh", "-c", "echo ran > /shared/poststart.txt"]
EOF
    wait_until 30 "$name Running" pod_is_phase "$name" Running
    local content
    content="$(wait_for_check_file "$name" shared poststart.txt 30)"
    assert_eq "$content" "ran" "postStart hook output"
    delete_pod_if_exists "$name"
}

test_prestop_hook_runs_before_termination() {
    if ! node_uses_cri_runtime; then skip_test "needs cri runtime"; fi
    local name="prestop-hook"
    apply_manifest <<EOF
apiVersion: v1
kind: Pod
metadata:
  name: $name
spec:
  terminationGracePeriodSeconds: 15
  volumes:
    - name: shared
      emptyDir: {}
  containers:
    - name: app
      image: $TEST_IMAGE
      command: ["sleep", "3600"]
      volumeMounts:
        - name: shared
          mountPath: /shared
      lifecycle:
        preStop:
          exec:
            command: ["sh", "-c", "echo ran > /shared/prestop.txt"]
EOF
    wait_until 30 "$name Running" pod_is_phase "$name" Running
    kctl delete pod "$name" --wait=false >/dev/null
    local content
    content="$(wait_for_check_file "$name" shared prestop.txt 20)"
    assert_eq "$content" "ran" "preStop hook output"
    wait_until 30 "$name gone" pod_gone "$name"
}

test_termination_grace_period_is_honored_not_instant() {
    if ! node_uses_cri_runtime; then skip_test "needs cri runtime"; fi
    local name="grace-period"
    apply_manifest <<EOF
apiVersion: v1
kind: Pod
metadata:
  name: $name
spec:
  terminationGracePeriodSeconds: 8
  containers:
    - name: app
      image: $TEST_IMAGE
      # Traps SIGTERM and keeps running instead of exiting immediately,
      # so the only way it goes away within the window is SIGKILL at the
      # end of the grace period. NOT 'sleep 3600 & wait' (and deliberately
      # not backticked here — this whole block is inside an unquoted
      # heredoc, where backticks trigger real command substitution even
      # on a '#' line, since heredoc bodies aren't parsed as bash source)
      # — ash/dash's 'wait' returns as soon as a trap fires, whether or
      # not the backgrounded child actually exited, so that idiom's shell
      # (the container's PID 1) would exit voluntarily within
      # milliseconds of SIGTERM despite the trap "running" — proving
      # nothing about grace periods. A foreground loop has no such
      # wait-interrupt escape hatch: dash still runs the trap and returns
      # to the loop, so only an actual SIGKILL ends it.
      command: ["sh", "-c", "trap 'echo trapped' TERM; while true; do sleep 1; done"]
EOF
    wait_until 30 "$name Running" pod_is_phase "$name" Running
    local start
    start=$(date +%s)
    kctl delete pod "$name" --wait=false >/dev/null
    wait_until 40 "$name gone" pod_gone "$name"
    local elapsed=$(( $(date +%s) - start ))
    # Not a tight bound (scheduling/CRI overhead varies) — just proves this
    # wasn't torn down instantly (grace period ignored) or hung forever.
    [[ "$elapsed" -ge 5 ]] || die "pod disappeared in ${elapsed}s — terminationGracePeriodSeconds (8s) doesn't look honored"
    [[ "$elapsed" -le 35 ]] || die "pod took ${elapsed}s to disappear — grace period handling looks stuck, not bounded"
}

register_test test_poststart_hook_runs_after_container_start
register_test test_prestop_hook_runs_before_termination
register_test test_termination_grace_period_is_honored_not_instant
