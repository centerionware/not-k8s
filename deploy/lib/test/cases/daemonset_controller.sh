# lib/test/cases/daemonset_controller.sh — nodecontroller's Group E:
# daemonset-controller. Single-node e2e harness, so this can't prove
# "one Pod per Node" across a fleet — it proves the part that's still real
# on one node: the controller places a Pod directly (spec.nodeName set at
# creation, bypassing nodescheduler entirely) and keeps status current.

_nodecontroller_is_running_ds() {
    if command -v systemctl >/dev/null 2>&1; then
        systemctl is-active --quiet nodecontroller 2>/dev/null && return 0
    fi
    pgrep -x nodecontroller >/dev/null 2>&1
}

_require_nodecontroller_ds() {
    _nodecontroller_is_running_ds \
        || skip_test "nodecontroller isn't running here — deploy with --controller-manager=nodecontroller (which also disables k3s's own controller manager) to exercise these"
}

test_daemonset_places_a_pod_directly() {
    _require_nodecontroller_ds
    local ds="ds-test"
    local this_node
    this_node=$(kctl get nodes -o jsonpath='{.items[0].metadata.name}')

    apply_manifest <<EOF
apiVersion: apps/v1
kind: DaemonSet
metadata:
  name: $ds
spec:
  selector:
    matchLabels:
      app: $ds
  template:
    metadata:
      labels:
        app: $ds
    spec:
      containers:
        - name: busybox
          image: busybox:latest
          command: ["sleep", "3600"]
EOF
    trap 'kctl delete daemonset "$ds" --ignore-not-found >/dev/null 2>&1 || true; kctl delete pods -l "app=$ds" --ignore-not-found >/dev/null 2>&1 || true' EXIT

    wait_until 60 "daemonset $ds places a Pod on $this_node directly" \
        bash -c "[[ \"\$(kctl get pods -l 'app=$ds' -o jsonpath='{.items[0].spec.nodeName}')\" == '$this_node' ]]"

    wait_until 90 "daemonset $ds reports numberReady=1" \
        bash -c "[[ \"\$(kctl get daemonset '$ds' -o jsonpath='{.status.numberReady}')\" == '1' ]]"

    wait_until 30 "daemonset $ds reports desiredNumberScheduled=1" \
        bash -c "[[ \"\$(kctl get daemonset '$ds' -o jsonpath='{.status.desiredNumberScheduled}')\" == '1' ]]"

    trap - EXIT
    kctl delete daemonset "$ds" --ignore-not-found >/dev/null 2>&1 || true
    kctl delete pods -l "app=$ds" --ignore-not-found >/dev/null 2>&1 || true
}

register_test test_daemonset_places_a_pod_directly
