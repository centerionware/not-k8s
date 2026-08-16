# lib/test/cases/garbage_collector_controller.sh — nodecontroller's Group D:
# garbage-collector-controller. Proves owner-reference cascade deletion end
# to end: deleting a Deployment must cascade-delete its ReplicaSet, which
# must itself cascade-delete its Pods — not orphan them, which is what
# happens with no garbage-collector-controller running at all.

_nodecontroller_is_running_gc() {
    if command -v systemctl >/dev/null 2>&1; then
        systemctl is-active --quiet nodecontroller 2>/dev/null && return 0
    fi
    pgrep -x nodecontroller >/dev/null 2>&1
}

_require_nodecontroller_gc() {
    _nodecontroller_is_running_gc \
        || skip_test "nodecontroller isn't running here — deploy with --controller-manager=nodecontroller (which also disables k3s's own controller manager) to exercise these"
}

_gc_replicaset_count() { # _gc_replicaset_count <deployment-name>
    kctl get replicasets -l "app=$1" --no-headers 2>/dev/null | wc -l | tr -d ' '
}
export -f _gc_replicaset_count

_gc_pod_count() { # _gc_pod_count <deployment-name>
    kctl get pods -l "app=$1" --no-headers 2>/dev/null | wc -l | tr -d ' '
}
export -f _gc_pod_count

test_garbage_collector_cascades_deployment_delete_to_replicaset_and_pods() {
    _require_nodecontroller_gc
    local dep="gc-test"

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
        bash -c "[[ \"\$(_gc_replicaset_count '$dep')\" == '1' ]]"

    wait_until 60 "deployment $dep has 2 Pods" \
        bash -c "[[ \"\$(_gc_pod_count '$dep')\" == '2' ]]"

    # Delete only the Deployment — nothing here deletes the ReplicaSet or
    # Pods directly. Without garbage-collector-controller these orphan
    # forever.
    kctl delete deployment "$dep" --ignore-not-found >/dev/null 2>&1

    wait_until 60 "deleting deployment $dep cascades to its ReplicaSet" \
        bash -c "[[ \"\$(_gc_replicaset_count '$dep')\" == '0' ]]"

    wait_until 60 "deleting deployment $dep cascades to its Pods" \
        bash -c "[[ \"\$(_gc_pod_count '$dep')\" == '0' ]]"

    trap - EXIT
}

register_test test_garbage_collector_cascades_deployment_delete_to_replicaset_and_pods
