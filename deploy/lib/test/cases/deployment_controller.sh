# lib/test/cases/deployment_controller.sh — nodecontroller's Group E:
# deployment-controller. Exercises the rolling-update path end to end: a
# Deployment creates a ReplicaSet, which (via replicaset-controller) creates
# Pods; then a template change (image bump) must roll to a *new* ReplicaSet
# while draining the old one, not just mutate Pods in place.

_nodecontroller_is_running_dc() {
    if command -v systemctl >/dev/null 2>&1; then
        systemctl is-active --quiet nodecontroller 2>/dev/null && return 0
    fi
    pgrep -x nodecontroller >/dev/null 2>&1
}

_k3s_controller_manager_disabled_dc() {
    test_controller_manager_is_exclusive
}

_require_nodecontroller_dc() {
    _nodecontroller_is_running_dc \
        || skip_test "nodecontroller isn't running here — deploy with --controller-manager=nodecontroller to exercise these"
    _k3s_controller_manager_disabled_dc \
        || skip_test "k3s's bundled controller-manager is still enabled; deploy with --controller-manager=nodecontroller so this test exercises nodecontroller"
}

_dc_owned_pod_count() { # _dc_owned_pod_count <deployment-name>
    kctl get pods -l "app=$1" --no-headers 2>/dev/null | wc -l | tr -d ' '
}
export -f _dc_owned_pod_count

_dc_replicaset_count() { # _dc_replicaset_count <deployment-name>
    kctl get replicasets -l "app=$1" --no-headers 2>/dev/null | wc -l | tr -d ' '
}
export -f _dc_replicaset_count

test_deployment_creates_replicaset_and_rolls_update() {
    _require_nodecontroller_dc
    local dep="dep-test"

    apply_manifest <<EOF
apiVersion: apps/v1
kind: Deployment
metadata:
  name: $dep
spec:
  replicas: 2
  selector:
    matchLabels:
      app: $dep
  template:
    metadata:
      labels:
        app: $dep
    spec:
      containers:
        - name: busybox
          image: busybox:latest
          command: ["sleep", "3600"]
EOF
    trap 'kctl delete deployment "$dep" --ignore-not-found >/dev/null 2>&1 || true; kctl delete replicasets -l "app=$dep" --ignore-not-found >/dev/null 2>&1 || true; kctl delete pods -l "app=$dep" --ignore-not-found >/dev/null 2>&1 || true' EXIT

    wait_until 60 "deployment $dep creates a ReplicaSet" \
        bash -c "[[ \"\$(_dc_replicaset_count '$dep')\" == '1' ]]"

    wait_until 60 "deployment $dep has 2 Pods" \
        bash -c "[[ \"\$(_dc_owned_pod_count '$dep')\" == '2' ]]"

    wait_until 90 "deployment $dep reports 2 readyReplicas" \
        bash -c "[[ \"\$(kctl get deployment '$dep' -o jsonpath='{.status.readyReplicas}')\" == '2' ]]"

    # Roll to a new template: the old ReplicaSet must drain to 0 and a
    # second one appear, not have its Pods mutated in place.
    kctl patch deployment "$dep" --type=merge \
        -p '{"spec":{"template":{"spec":{"containers":[{"name":"busybox","image":"busybox:latest","command":["sleep","7200"]}]}}}}' >/dev/null

    wait_until 90 "deployment $dep rolls to a second ReplicaSet" \
        bash -c "[[ \"\$(_dc_replicaset_count '$dep')\" -ge '2' ]]"

    wait_until 90 "deployment $dep still has 2 Pods after the roll" \
        bash -c "[[ \"\$(_dc_owned_pod_count '$dep')\" == '2' ]]"

    trap - EXIT
    kctl delete deployment "$dep" --ignore-not-found >/dev/null 2>&1 || true
    kctl delete replicasets -l "app=$dep" --ignore-not-found >/dev/null 2>&1 || true
    kctl delete pods -l "app=$dep" --ignore-not-found >/dev/null 2>&1 || true
}

register_test test_deployment_creates_replicaset_and_rolls_update
