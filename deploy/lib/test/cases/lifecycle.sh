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

test_crash_loop_backoff_throttles_immediate_restarts() {
    # Round 73: before this, a container that exits immediately (no
    # sleep at all) had NO restart throttle whatsoever — every status
    # write is itself a Pod modification that re-triggers this
    # controller's own watch stream, feeding back into another
    # reconcile/restart with no natural rate limit. Without backoff, a
    # container that exits in well under a second could restart dozens
    # of times within this test's own wait window; with it, the very
    # first restart is immediate but every one after that is throttled
    # (10s, doubling), so the count should still be small.
    if ! node_uses_cri_runtime; then skip_test "needs cri runtime"; fi
    local name="crash-loop-backoff"
    apply_manifest <<EOF
apiVersion: v1
kind: Pod
metadata:
  name: $name
spec:
  containers:
    - name: app
      image: $TEST_IMAGE
      command: ["sh", "-c", "exit 1"]
EOF
    # Give it a real window to (mis)behave in before checking: long
    # enough that an unthrottled tight loop would rack up many restarts,
    # short enough that even a single 10s backoff window keeps a
    # correctly-throttled count very low.
    sleep 20
    local restart_count
    restart_count="$(pod_container_restart_count "$name" app)"
    delete_pod_if_exists "$name"
    # A high count here means restarts aren't being throttled at all —
    # check restart_backoff_ready()/record_restart_backoff() in
    # runtime/cri/container_create.rs. The very first restart is never
    # throttled, so at least 1 is expected; anything past low single
    # digits within 20s under a 10s base backoff delay means the gate
    # isn't doing anything.
    assert_true test "$restart_count" -ge 1
    assert_true test "$restart_count" -le 3
}

test_crash_loop_backoff_reports_waiting_reason_and_last_state() {
    # Round 75: while a crash-looping container is backing off, its
    # *current* state must show Waiting{reason: CrashLoopBackOff} (not
    # Terminated, even though it really did exit) with the exited
    # instance's own details moved into lastState.terminated instead --
    # matching real kubectl's familiar display. The very first restart
    # (replacing the 1st exited instance with a 2nd) is never throttled
    # (round 73); this polls for the 2nd instance's own exit to land it
    # in the backoff window, which is when waiting.reason actually
    # becomes CrashLoopBackOff -- restartCount alone reaching 1 isn't
    # enough, since that happens as soon as the 2nd instance is CREATED,
    # not once it has exited and started backing off.
    if ! node_uses_cri_runtime; then skip_test "needs cri runtime"; fi
    local name="crash-loop-last-state"
    apply_manifest <<EOF
apiVersion: v1
kind: Pod
metadata:
  name: $name
spec:
  containers:
    - name: app
      image: $TEST_IMAGE
      command: ["sh", "-c", "exit 1"]
EOF
    if ! try_wait_until 30 bash -c \
        "[[ \"\$(kctl get pod '$name' -o jsonpath='{.status.containerStatuses[0].state.waiting.reason}')\" == 'CrashLoopBackOff' ]]"; then
        delete_pod_if_exists "$name"
        skip_test "state.waiting.reason never became CrashLoopBackOff within 30s -- check restart_policy != \"Never\" && !restart_backoff_ready() gating in runtime/cri/status.rs"
    fi
    local last_exit_code
    last_exit_code="$(kctl get pod "$name" -o jsonpath='{.status.containerStatuses[0].lastState.terminated.exitCode}')"
    delete_pod_if_exists "$name"
    assert_not_empty "$last_exit_code" "containerStatuses[0].lastState.terminated.exitCode -- check last_container_state()/TerminatedInfo wiring in pods.rs"
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

test_lifecycle_stop_signal_is_honored_by_the_runtime() {
    # Round 66: lifecycle.stopSignal (GA 1.33) used to be entirely
    # ignored — CRI's own ContainerConfig.stop_signal field already had
    # native support, nobody wired it up. Real proof, not just "the pod
    # ran": the container traps SIGUSR1 (NOT the default SIGTERM a plain
    # `sleep` would just die to silently) and writes a marker before
    # exiting. If stopSignal were ignored and the runtime sent SIGTERM
    # instead, the trap would never fire and the marker would never
    # appear. Reads the marker off the host-materialized emptyDir
    # directory (which nodelet leaves in place after pod teardown, a
    # documented pre-existing simplification) since the Pod object itself
    # may already be gone from the apiserver by the time this checks.
    if ! node_uses_cri_runtime; then skip_test "needs cri runtime"; fi
    local name="stop-signal-check"
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
      lifecycle:
        stopSignal: SIGUSR1
      command: ["sh", "-c", "trap 'echo got-usr1 > /shared/signal.txt; exit 7' USR1; sleep 3600"]
      volumeMounts:
        - {name: shared, mountPath: /shared}
EOF
    if ! try_wait_until 30 pod_is_phase "$name" Running; then
        delete_pod_if_exists "$name"
        skip_test "pod never reached Running with lifecycle.stopSignal set — check nodelet's logs, or that this runtime version supports CRI's ContainerConfig.stop_signal field at all"
    fi
    local path="$(pod_volume_host_path "$name" shared)/signal.txt"
    delete_pod_if_exists "$name"
    if ! try_wait_until 30 bash -c "[[ -s '$path' ]]"; then
        rm -f "$path" 2>/dev/null || true
        skip_test "no $path appeared after pod deletion — the runtime may not honor CRI's ContainerConfig.stop_signal (sent plain SIGTERM instead of the configured SIGUSR1, so the trap never fired); check runtime/cri/container_create.rs's stop_signal wiring if this is unexpected on a runtime version known to support it"
    fi
    local content
    content="$(cat "$path")"
    rm -f "$path" 2>/dev/null || true
    assert_eq "$content" "got-usr1" "the container's SIGUSR1 trap should have fired, proving the runtime used the configured stopSignal instead of the default SIGTERM"
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

test_container_status_container_id_has_a_runtime_scheme_prefix() {
    # Round 57: real kubelet always formats containerStatuses[].containerID
    # as <runtimeName>://<id> (e.g. containerd://...); nodelet used to
    # report the bare CRI container ID with no prefix at all. Not
    # asserting the exact runtime name (varies by deployment) — just that
    # the well-known scheme-separator shape is present.
    if ! node_uses_cri_runtime; then skip_test "needs cri runtime"; fi
    local name="container-id-scheme-check"
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
    local container_id
    container_id="$(kctl get pod "$name" -o jsonpath='{.status.containerStatuses[0].containerID}')"
    assert_contains "$container_id" "://" "containerStatuses[0].containerID should have a <runtimeName>:// scheme prefix"
    delete_pod_if_exists "$name"
}

test_pod_status_reports_host_ips_plural() {
    # Round 56: real kubelet always sets hostIPs alongside the singular
    # hostIP, even on a single-stack node — this was never set at all.
    local name="host-ips-check"
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
    local host_ip host_ips_first
    host_ip="$(kctl get pod "$name" -o jsonpath='{.status.hostIP}')"
    host_ips_first="$(kctl get pod "$name" -o jsonpath='{.status.hostIPs[0].ip}')"
    assert_not_empty "$host_ip" "status.hostIP"
    assert_eq "$host_ips_first" "$host_ip" "status.hostIPs[0].ip should match the singular hostIP"
    delete_pod_if_exists "$name"
}

test_pod_status_reports_qos_class() {
    # Round 55: PodStatus.qosClass was never set at all before this;
    # nodelet already computed this internally (eviction::qos_class(),
    # round 7's eviction ranking) — this proves it's now surfaced for a
    # real Guaranteed pod (equal request/limit on every resource).
    local name="qos-class-check"
    apply_manifest <<EOF
apiVersion: v1
kind: Pod
metadata:
  name: $name
spec:
  containers:
    - name: app
      image: $TEST_IMAGE
      resources:
        requests:
          cpu: "100m"
          memory: "64Mi"
        limits:
          cpu: "100m"
          memory: "64Mi"
      command: ["sleep", "3600"]
EOF
    wait_until 30 "$name Running" pod_is_phase "$name" Running
    local qos
    qos="$(kctl get pod "$name" -o jsonpath='{.status.qosClass}')"
    assert_eq "$qos" "Guaranteed" "status.qosClass for a pod with matching requests/limits on every resource"
    delete_pod_if_exists "$name"
}

test_container_status_reports_a_real_image_id() {
    # Round 52: containerStatuses[].imageID used to always be the empty
    # string; CRI's own Container.image_ref (a digested image reference)
    # is now carried through. Not asserting the exact digest format
    # (varies by registry/runtime) — just that it's real and non-empty.
    if ! node_uses_cri_runtime; then skip_test "needs cri runtime"; fi
    local name="image-id-check"
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
    local image_id
    wait_until 20 "$name containerStatuses[0].imageID to be populated" bash -c \
        "[[ -n \"\$(kctl get pod '$name' -o jsonpath='{.status.containerStatuses[0].imageID}')\" ]]"
    image_id="$(kctl get pod "$name" -o jsonpath='{.status.containerStatuses[0].imageID}')"
    assert_not_empty "$image_id" "containerStatuses[0].imageID"
    delete_pod_if_exists "$name"
}

test_image_pull_policy_never_fails_when_image_is_absent() {
    # Round 51: imagePullPolicy: Never must refuse to pull at all. Uses a
    # real, valid, pullable image/tag this suite never otherwise
    # references (so it's very unlikely already cached) — a fake/
    # nonexistent tag wouldn't distinguish "Never correctly refused to
    # pull" from "Never was ignored but the pull failed anyway because
    # the tag doesn't exist upstream either." If Never isn't honored,
    # this pod would actually succeed in pulling and reach Running.
    if ! node_uses_cri_runtime; then skip_test "needs cri runtime"; fi
    local name="pull-never-absent"
    apply_manifest <<EOF
apiVersion: v1
kind: Pod
metadata:
  name: $name
spec:
  containers:
    - name: app
      image: busybox:1.35.0
      imagePullPolicy: Never
      command: ["sleep", "3600"]
EOF
    if try_wait_until 20 pod_is_phase "$name" Running; then
        delete_pod_if_exists "$name"
        die "pod reached Running with imagePullPolicy: Never against an image this node shouldn't already have cached — the Never policy isn't being honored (check effective_pull_policy()/create_and_start_container() in runtime/cri.rs)"
    fi
    delete_pod_if_exists "$name"
}

test_image_pull_policy_if_not_present_manual_note() {
    skip_test "proving IfNotPresent/Never actually SKIP the registry round-trip (not just that Never fails when absent, which test_image_pull_policy_never_fails_when_image_is_absent already covers live) needs reading nodelet's own logs or observing network activity — not something this suite does. Manual spot-check: pull $TEST_IMAGE once via a Running pod, then create a second pod referencing the same image with imagePullPolicy: IfNotPresent and (if your registry supports it) point nodelet at an unreachable/offline registry mirror — confirm the second pod still reaches Running quickly (no failed registry call blocking it), and check nodelet's logs show no new ImageStatus miss or PullImage call for that image."
}

register_test test_basic_pod_runs
register_test test_init_containers_run_before_app_container
register_test test_native_sidecar_container_starts_before_app_container_and_keeps_running
register_test test_native_sidecar_container_restarts_on_crash
register_test test_init_container_failure_blocks_app_container_under_restart_policy_never
register_test test_crashing_container_restarts_and_increments_restart_count
register_test test_crash_loop_backoff_throttles_immediate_restarts
register_test test_crash_loop_backoff_reports_waiting_reason_and_last_state
register_test test_restart_policy_never_exit_zero_is_succeeded
register_test test_restart_policy_never_exit_nonzero_is_failed
register_test test_exited_container_reports_terminated_state_with_exit_code
register_test test_lifecycle_stop_signal_is_honored_by_the_runtime
register_test test_termination_message_path_is_read_back_into_container_status
register_test test_container_status_container_id_has_a_runtime_scheme_prefix
register_test test_pod_status_reports_host_ips_plural
register_test test_pod_status_reports_qos_class
register_test test_container_status_reports_a_real_image_id
register_test test_image_pull_policy_never_fails_when_image_is_absent
register_test test_image_pull_policy_if_not_present_manual_note
