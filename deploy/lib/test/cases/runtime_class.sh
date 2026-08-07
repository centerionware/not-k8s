# lib/test/cases/runtime_class.sh — spec.runtimeClassName -> CRI's
# runtime_handler (resolve_runtime_handler() in runtime/cri.rs). Real
# alternative-runtime handlers (gVisor's "runsc", Kata's "kata") aren't
# something this suite can assume are installed, so this only proves the
# *lookup and wiring* works, using whatever handler name this containerd is
# already configured with for its default runtime (defaults to "runc",
# which every real containerd install has) — override
# TEST_RUNTIME_CLASS_HANDLER if a given host genuinely configures
# something else. Round 123: a pod that fails to reach Running here is a
# hard failure, not a skip — "runc" is never actually absent, so this
# almost always means real handler-resolution wiring is broken.

test_runtime_class_handler_is_honored() {
    if ! node_uses_cri_runtime; then skip_test "needs cri runtime"; fi
    local handler="${TEST_RUNTIME_CLASS_HANDLER:-runc}"
    local rc_name="nodelet-e2e-runtimeclass"
    local name="runtimeclass-check"

    if ! kubectl get runtimeclasses >/dev/null 2>&1; then
        skip_test "RuntimeClass API not available on this cluster"
    fi

    cat <<EOF | kubectl apply -f - >/dev/null 2>&1 || { skip_test "couldn't create a RuntimeClass (admission/RBAC?)"; }
apiVersion: node.k8s.io/v1
kind: RuntimeClass
metadata:
  name: $rc_name
handler: $handler
EOF

    apply_manifest <<EOF
apiVersion: v1
kind: Pod
metadata:
  name: $name
spec:
  runtimeClassName: $rc_name
  containers:
    - name: app
      image: $TEST_IMAGE
      command: ["sleep", "3600"]
EOF

    if ! try_wait_until 30 pod_is_phase "$name" Running; then
        delete_pod_if_exists "$name"
        kubectl delete runtimeclass "$rc_name" --ignore-not-found >/dev/null 2>&1
        die "pod never reached Running with runtimeClassName=$rc_name (handler '$handler') — set TEST_RUNTIME_CLASS_HANDLER to a handler name this containerd actually has configured, or this containerd may only have the implicit default with no named handlers at all"
    fi

    delete_pod_if_exists "$name"
    kubectl delete runtimeclass "$rc_name" --ignore-not-found >/dev/null 2>&1
}

register_test test_runtime_class_handler_is_honored
