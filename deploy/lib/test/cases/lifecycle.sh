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

register_test test_basic_pod_runs
register_test test_init_containers_run_before_app_container
register_test test_init_container_failure_blocks_app_container_under_restart_policy_never
register_test test_crashing_container_restarts_and_increments_restart_count
register_test test_restart_policy_never_exit_zero_is_succeeded
register_test test_restart_policy_never_exit_nonzero_is_failed
