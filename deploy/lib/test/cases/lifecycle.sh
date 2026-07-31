# lib/test/cases/lifecycle.sh — basic pod lifecycle, init containers,
# crash-restart + restart counts, restartPolicy: Never exit-code phase.

test_basic_pod_runs() {
    local name="basic-pod"
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
    wait_until 60 "$name Running" pod_is_phase "$name" Running
    wait_until 30 "$name container ready" pod_container_ready "$name" app
    assert_eq "$(pod_condition_status "$name" Ready)" "True" "Ready condition"
    delete_pod_if_exists "$name"
}

test_init_containers_run_before_app_container() {
    if ! node_uses_cri_runtime; then skip_test "needs cri runtime"; fi
    local name="init-order"
    apply_manifest <<EOF
apiVersion: v1
kind: Pod
metadata:
  name: $name
spec:
  initContainers:
    - name: init-one
      image: $TEST_IMAGE
      command: ["sh", "-c", "sleep 2"]
    - name: init-two
      image: $TEST_IMAGE
      command: ["sh", "-c", "sleep 2"]
  containers:
    - name: app
      image: $TEST_IMAGE
      command: ["sleep", "3600"]
EOF
    # Structural proof, not just a status string: nodelet's ensure_init_containers()
    # gates app-container creation on every init container having exited zero, in
    # order — so the app container ever reaching Running is only possible if both
    # init containers already completed successfully.
    wait_until 90 "$name Running" pod_is_phase "$name" Running
    assert_eq "$(pod_condition_status "$name" Initialized)" "True" "Initialized condition"

    local statuses
    statuses="$(kctl get pod "$name" -o jsonpath='{.status.initContainerStatuses[*].name}')"
    assert_eq "$statuses" "init-one init-two" "initContainerStatuses order"
    delete_pod_if_exists "$name"
}

test_native_sidecar_container_starts_before_app_container_and_keeps_running() {
    # Round 36: initContainers[].restartPolicy: Always. A sidecar that
    # never exits must not block the app container the way a regular init
    # container would — real structural proof, not just a status string.
    if ! node_uses_cri_runtime; then skip_test "needs cri runtime"; fi
    local name="native-sidecar"
    apply_manifest <<EOF
apiVersion: v1
kind: Pod
metadata:
  name: $name
spec:
  initContainers:
    - name: proxy
      image: $TEST_IMAGE
      restartPolicy: Always
      command: ["sleep", "3600"]
  containers:
    - name: app
      image: $TEST_IMAGE
      command: ["sleep", "3600"]
EOF
    wait_until 30 "$name Running" pod_is_phase "$name" Running
    assert_eq "$(pod_condition_status "$name" Initialized)" "True" "Initialized condition"

    local sidecar_state
    sidecar_state="$(kctl get pod "$name" -o jsonpath='{.status.initContainerStatuses[0].state.running}')"
    assert_not_empty "$sidecar_state" "sidecar's own initContainerStatuses entry should show state.running (it never exits, unlike a regular init container)"

    delete_pod_if_exists "$name"
}

test_native_sidecar_container_restarts_on_crash() {
    if ! node_uses_cri_runtime; then skip_test "needs cri runtime"; fi
    local name="native-sidecar-crash"
    apply_manifest <<EOF
apiVersion: v1
kind: Pod
metadata:
  name: $name
spec:
  initContainers:
    - name: proxy
      image: $TEST_IMAGE
      restartPolicy: Always
      command: ["sh", "-c", "sleep 3; exit 1"]
  containers:
    - name: app
      image: $TEST_IMAGE
      command: ["sleep", "3600"]
EOF
    wait_until 30 "$name Running" pod_is_phase "$name" Running
    # The sidecar crash-loops every ~3s — its own restart count (not the
    # app container's) must climb above zero, same restart-count
    # mechanism ensure_container()'s app-container path already uses.
    wait_until 60 "sidecar restart count > 0" bash -c \
        "[[ \"\$(kctl get pod '$name' -o jsonpath='{.status.initContainerStatuses[0].restartCount}')\" -gt 0 ]]"
    delete_pod_if_exists "$name"
}

test_init_container_failure_blocks_app_container_under_restart_policy_never() {
    if ! node_uses_cri_runtime; then skip_test "needs cri runtime"; fi
    local name="init-fail-never"
    apply_manifest <<EOF
apiVersion: v1
kind: Pod
metadata:
  name: $name
spec:
  restartPolicy: Never
  initContainers:
    - name: doomed
      image: $TEST_IMAGE
      command: ["sh", "-c", "exit 7"]
  containers:
    - name: app
      image: $TEST_IMAGE
      command: ["sleep", "3600"]
EOF
    wait_until 60 "$name Failed" pod_is_phase "$name" Failed
    delete_pod_if_exists "$name"
}

test_crashing_container_restarts_and_increments_restart_count() {
    if ! node_uses_cri_runtime; then skip_test "needs cri runtime"; fi
    local name="crash-loop"
    apply_manifest <<EOF
apiVersion: v1
kind: Pod
metadata:
  name: $name
spec:
  containers:
    - name: app
      image: $TEST_IMAGE
      command: ["sh", "-c", "sleep 3; exit 1"]
EOF
    # restartPolicy defaults to Always — the container should crash and come
    # back at least once, bumping restartCount above zero.
    wait_until 90 "restart count > 0" bash -c \
        "[[ \"\$(pod_container_restart_count '$name' app)\" -gt 0 ]]"
    delete_pod_if_exists "$name"
}

test_restart_policy_never_exit_zero_is_succeeded() {
    if ! node_uses_cri_runtime; then skip_test "needs cri runtime"; fi
    local name="never-succeed"
    apply_manifest <<EOF
apiVersion: v1
kind: Pod
metadata:
  name: $name
spec:
  restartPolicy: Never
  containers:
    - name: app
      image: $TEST_IMAGE
      command: ["sh", "-c", "exit 0"]
EOF
    wait_until 60 "$name Succeeded" pod_is_phase "$name" Succeeded
    delete_pod_if_exists "$name"
}

test_restart_policy_never_exit_nonzero_is_failed() {
    if ! node_uses_cri_runtime; then skip_test "needs cri runtime"; fi
    local name="never-fail"
    apply_manifest <<EOF
apiVersion: v1
kind: Pod
metadata:
  name: $name
spec:
  restartPolicy: Never
  containers:
    - name: app
      image: $TEST_IMAGE
      command: ["sh", "-c", "exit 1"]
EOF
    # This is the exact round-3 fix: Never used to always report Succeeded
    # regardless of exit code.
    wait_until 60 "$name Failed" pod_is_phase "$name" Failed
    delete_pod_if_exists "$name"
}

test_exited_container_reports_terminated_state_with_exit_code() {
    # Round 24: exited containers used to always show "Waiting:
    # ContainerCreating" forever, never a real Terminated state.
    if ! node_uses_cri_runtime; then skip_test "needs cri runtime"; fi
    local name="terminated-state-check"
    apply_manifest <<EOF
apiVersion: v1
kind: Pod
metadata:
  name: $name
spec:
  restartPolicy: Never
  containers:
    - name: app
      image: $TEST_IMAGE
      command: ["sh", "-c", "exit 3"]
EOF
    wait_until 60 "$name Failed" pod_is_phase "$name" Failed

    local exit_code reason
    exit_code="$(kctl get pod "$name" -o jsonpath='{.status.containerStatuses[0].state.terminated.exitCode}')"
    reason="$(kctl get pod "$name" -o jsonpath='{.status.containerStatuses[0].state.terminated.reason}')"
    assert_eq "$exit_code" "3" "containerStatuses[0].state.terminated.exitCode"
    assert_not_empty "$reason" "containerStatuses[0].state.terminated.reason (e.g. Error/Completed, or the runtime's own OOMKilled etc.)"
    delete_pod_if_exists "$name"
}

test_termination_message_path_is_read_back_into_container_status() {
    # Round 24: terminationMessagePath was threaded through to CRI's
    # ContainerConfig struct copy but never actually bind-mounted or read
    # back — this proves both the mount and the read-back work end to end.
    if ! node_uses_cri_runtime; then skip_test "needs cri runtime"; fi
    local name="termination-message-check"
    apply_manifest <<EOF
apiVersion: v1
kind: Pod
metadata:
  name: $name
spec:
  restartPolicy: Never
  containers:
    - name: app
      image: $TEST_IMAGE
      command: ["sh", "-c", "echo -n 'disk quota exceeded' > /dev/termination-log; exit 1"]
EOF
    wait_until 60 "$name Failed" pod_is_phase "$name" Failed

    local message
    message="$(kctl get pod "$name" -o jsonpath='{.status.containerStatuses[0].state.terminated.message}')"
    assert_eq "$message" "disk quota exceeded" "containerStatuses[0].state.terminated.message — check the termination-log host bind mount and read_termination_message() in runtime/cri.rs"
    delete_pod_if_exists "$name"
}

register_test test_basic_pod_runs
register_test test_init_containers_run_before_app_container
register_test test_native_sidecar_container_starts_before_app_container_and_keeps_running
register_test test_native_sidecar_container_restarts_on_crash
register_test test_init_container_failure_blocks_app_container_under_restart_policy_never
register_test test_crashing_container_restarts_and_increments_restart_count
register_test test_restart_policy_never_exit_zero_is_succeeded
register_test test_restart_policy_never_exit_nonzero_is_failed
register_test test_exited_container_reports_terminated_state_with_exit_code
register_test test_termination_message_path_is_read_back_into_container_status
