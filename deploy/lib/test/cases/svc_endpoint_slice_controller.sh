# lib/test/cases/endpoint_slice_controller.sh — nodecontroller's Group B:
# endpointslice-controller. No e2e coverage anywhere else in this suite —
# until nodecontroller existed this was entirely k3s's bundled
# controller-manager's job (see docs/CONTROLLER_MANAGER.md, Group B).
#
# Gated on CONTROLLER_MANAGER=nodecontroller actually running, the same
# "check the unit, not the binary's presence" discipline every other
# nodecontroller test file in this suite uses.

_nodecontroller_is_running_es() {
    if command -v systemctl >/dev/null 2>&1; then
        systemctl is-active --quiet nodecontroller 2>/dev/null && return 0
    fi
    pgrep -x nodecontroller >/dev/null 2>&1
}

_require_nodecontroller_es() {
    _nodecontroller_is_running_es \
        || skip_test "nodecontroller isn't running here — deploy with --controller-manager=nodecontroller (which also disables k3s's own controller manager) to exercise these"
}

endpointslice_addresses() { # endpointslice_addresses <slice-name>
    # Index-based, not a [*] wildcard chained into a further index — this
    # test's own real bug, found live in CI: kubectl's jsonpath silently
    # returns nothing for {.endpoints[*].addresses[0]} (a wildcard
    # followed by an index on each result), which made this test time out
    # for 60s on every run despite nodecontroller's own diagnostic logs
    # (endpoint_slice.rs) proving it had applied the correct EndpointSlice
    # — with the right address — within half a second every single time.
    # There's exactly one endpoint expected here, so index 0 directly.
    kctl get endpointslice "$1" -o jsonpath='{.endpoints[0].addresses[0]}' 2>/dev/null || true
}

test_endpointslice_is_produced_for_a_selected_pod() {
    _require_nodecontroller_es
    local svc="es-test-svc" pod="es-test-pod"

    apply_manifest <<EOF
apiVersion: v1
kind: Pod
metadata:
  name: $pod
  labels:
    app: $svc
spec:
  containers:
    - name: busybox
      image: busybox:latest
      command: ["sleep", "3600"]
EOF
    apply_manifest <<EOF
apiVersion: v1
kind: Service
metadata:
  name: $svc
spec:
  selector:
    app: $svc
  ports:
    - port: 80
      targetPort: 80
EOF

    trap 'delete_pod_if_exists "$pod"; kctl delete service "$svc" --ignore-not-found >/dev/null 2>&1 || true; kctl delete endpointslice "${svc}-nc" --ignore-not-found >/dev/null 2>&1 || true' EXIT

    wait_until 60 "pod $pod Running with an IP" \
        bash -c "[[ \"\$(pod_field '$pod' '{.status.phase}')\" == 'Running' && -n \"\$(pod_field '$pod' '{.status.podIP}')\" ]]"
    local pod_ip
    pod_ip="$(pod_field "$pod" '{.status.podIP}')"

    wait_until 60 "EndpointSlice ${svc}-nc carries $pod's address ($pod_ip)" \
        bash -c "[[ \"\$(endpointslice_addresses '${svc}-nc')\" == '$pod_ip' ]]"

    local ready
    ready="$(kctl get endpointslice "${svc}-nc" -o jsonpath='{.endpoints[0].conditions.ready}' 2>/dev/null)"
    assert_eq "$ready" "true" "the endpoint is marked ready once the pod is Running"

    delete_pod_and_wait_gone "$pod"
    wait_until 30 "EndpointSlice ${svc}-nc drops the deleted pod's address" \
        bash -c "[[ -z \"\$(endpointslice_addresses '${svc}-nc')\" ]]"

    trap - EXIT
    kctl delete service "$svc" --ignore-not-found >/dev/null 2>&1 || true
    kctl delete endpointslice "${svc}-nc" --ignore-not-found >/dev/null 2>&1 || true
}

register_test test_endpointslice_is_produced_for_a_selected_pod
