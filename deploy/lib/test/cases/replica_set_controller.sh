# lib/test/cases/replica_set_controller.sh — nodecontroller's Group E:
# replicaset-controller. No e2e coverage anywhere else in this suite —
# until nodecontroller existed this was entirely k3s's bundled
# controller-manager's job (see docs/CONTROLLER_MANAGER.md, Group E).

_nodecontroller_is_running_rs() {
    if command -v systemctl >/dev/null 2>&1; then
        systemctl is-active --quiet nodecontroller 2>/dev/null && return 0
    fi
    pgrep -x nodecontroller >/dev/null 2>&1
}

_k3s_controller_manager_disabled_rs() {
    test_controller_manager_is_exclusive
}

_require_nodecontroller_rs() {
    _nodecontroller_is_running_rs \
        || skip_test "nodecontroller isn't running here — deploy with --controller-manager=nodecontroller to exercise these"
    _k3s_controller_manager_disabled_rs \
        || skip_test "k3s's bundled controller-manager is still enabled; deploy with --controller-manager=nodecontroller so this test exercises nodecontroller"
}

_rs_owned_pod_count() { # _rs_owned_pod_count <replicaset-name>
    kctl get pods -l "app=$1" --no-headers 2>/dev/null | wc -l | tr -d ' '
}
export -f _rs_owned_pod_count

test_replicaset_creates_and_scales_pods() {
    _require_nodecontroller_rs
    local rs="rs-test"

    apply_manifest <<EOF
apiVersion: apps/v1
kind: ReplicaSet
metadata:
  name: $rs
spec:
  replicas: 2
  selector:
    matchLabels:
      app: $rs
  template:
    metadata:
      labels:
        app: $rs
    spec:
      containers:
        - name: busybox
          image: busybox:latest
          command: ["sleep", "3600"]
EOF
    trap 'kctl delete replicaset "$rs" --ignore-not-found >/dev/null 2>&1 || true; kctl delete pods -l "app=$rs" --ignore-not-found >/dev/null 2>&1 || true' EXIT

    wait_until 60 "replicaset $rs has 2 Pods" \
        bash -c "[[ \"\$(_rs_owned_pod_count '$rs')\" == '2' ]]"

    wait_until 90 "replicaset $rs reports 2 readyReplicas" \
        bash -c "[[ \"\$(kctl get replicaset '$rs' -o jsonpath='{.status.readyReplicas}')\" == '2' ]]"

    # Scale down: assert the controller deletes the excess Pod rather than
    # just updating the object's own spec (which anyone can do).
    kctl patch replicaset "$rs" --type=merge -p '{"spec":{"replicas":1}}' >/dev/null
    wait_until 60 "replicaset $rs scales down to 1 Pod" \
        bash -c "[[ \"\$(_rs_owned_pod_count '$rs')\" == '1' ]]"

    trap - EXIT
    kctl delete replicaset "$rs" --ignore-not-found >/dev/null 2>&1 || true
    kctl delete pods -l "app=$rs" --ignore-not-found >/dev/null 2>&1 || true
}

register_test test_replicaset_creates_and_scales_pods
