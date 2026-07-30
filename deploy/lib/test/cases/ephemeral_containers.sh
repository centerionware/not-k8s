# lib/test/cases/ephemeral_containers.sh — spec.ephemeralContainers, added
# post-hoc via the `ephemeralcontainers` subresource (what `kubectl debug`
# uses under the hood). Exercises: ensure_ephemeral_container() in
# runtime/cri.rs (create-once, never-restarted semantics), and
# PodStatus.ephemeralContainerStatuses being populated (pods.rs).

test_kubectl_debug_adds_and_starts_an_ephemeral_container() {
    if ! node_uses_cri_runtime; then skip_test "needs cri runtime"; fi
    local name="ephemeral-check"
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

    if ! kubectl debug "$name" -n "$TEST_NAMESPACE" --image="$TEST_IMAGE" --container=debugger -- sleep 3600 >/dev/null 2>&1; then
        delete_pod_if_exists "$name"
        skip_test "this kubectl/cluster version doesn't support 'kubectl debug' against a running pod (needs the ephemeralcontainers subresource)"
    fi

    if ! try_wait_until 30 bash -c "kubectl get pod '$name' -n '$TEST_NAMESPACE' -o jsonpath='{.status.ephemeralContainerStatuses[?(@.name==\"debugger\")].state.running}' | grep -q startedAt"; then
        delete_pod_if_exists "$name"
        die "ephemeralContainerStatuses never reported the debug container running — check ensure_ephemeral_container()/build_labeled_container_statuses() in runtime/cri.rs"
    fi

    # The pod's own phase/readiness must be unaffected by the debug
    # container's existence — it's app container 'app' that determines that.
    assert_eq "$(kubectl get pod "$name" -n "$TEST_NAMESPACE" -o jsonpath='{.status.phase}')" "Running" "pod phase unaffected by ephemeral container"

    delete_pod_if_exists "$name"
}

register_test test_kubectl_debug_adds_and_starts_an_ephemeral_container
