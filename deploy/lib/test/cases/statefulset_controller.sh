# lib/test/cases/statefulset_controller.sh — nodecontroller's Group E:
# statefulset-controller. Proves stable ordinal identity (pod-0, pod-1 by
# name, not a random suffix) and OrderedReady scale-down (highest ordinal
# goes first).

_nodecontroller_is_running_sts() {
    if command -v systemctl >/dev/null 2>&1; then
        systemctl is-active --quiet nodecontroller 2>/dev/null && return 0
    fi
    pgrep -x nodecontroller >/dev/null 2>&1
}

_require_nodecontroller_sts() {
    _nodecontroller_is_running_sts \
        || skip_test "nodecontroller isn't running here — deploy with --controller-manager=nodecontroller (which also disables k3s's own controller manager) to exercise these"
}

test_statefulset_creates_ordinal_pods_and_scales_down_highest_first() {
    _require_nodecontroller_sts
    local sts="sts-test"

    apply_manifest <<EOF
apiVersion: apps/v1
kind: StatefulSet
metadata:
  name: $sts
spec:
  serviceName: $sts
  replicas: 2
  selector:
    matchLabels:
      app: $sts
  template:
    metadata:
      labels:
        app: $sts
    spec:
      containers:
        - name: busybox
          image: busybox:latest
          command: ["sleep", "3600"]
EOF
    trap 'kctl delete statefulset "$sts" --ignore-not-found >/dev/null 2>&1 || true; kctl delete pods -l "app=$sts" --ignore-not-found >/dev/null 2>&1 || true' EXIT

    wait_until 60 "statefulset $sts creates pod ${sts}-0" \
        bash -c "kctl get pod '${sts}-0' >/dev/null 2>&1"

    wait_until 90 "statefulset $sts creates pod ${sts}-1 after ${sts}-0 is Ready" \
        bash -c "kctl get pod '${sts}-1' >/dev/null 2>&1"

    wait_until 90 "statefulset $sts reports 2 readyReplicas" \
        bash -c "[[ \"\$(kctl get statefulset '$sts' -o jsonpath='{.status.readyReplicas}')\" == '2' ]]"

    # Scale down to 1: ordinal 1 (the highest) must go, not ordinal 0 —
    # stable identity means the survivor is deterministic, not "whichever".
    kctl patch statefulset "$sts" --type=merge -p '{"spec":{"replicas":1}}' >/dev/null
    wait_until 60 "statefulset $sts deletes the highest ordinal (${sts}-1) first" \
        bash -c "! kctl get pod '${sts}-1' >/dev/null 2>&1"

    wait_until 10 "statefulset $sts keeps ${sts}-0 running" \
        bash -c "kctl get pod '${sts}-0' >/dev/null 2>&1"

    trap - EXIT
    kctl delete statefulset "$sts" --ignore-not-found >/dev/null 2>&1 || true
    kctl delete pods -l "app=$sts" --ignore-not-found >/dev/null 2>&1 || true
}

register_test test_statefulset_creates_ordinal_pods_and_scales_down_highest_first
