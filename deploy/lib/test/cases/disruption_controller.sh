# lib/test/cases/disruption_controller.sh — nodecontroller's Group J:
# disruption-controller. Proves PodDisruptionBudget.status gets computed
# for real from live Pods, not left stale — the apiserver's own eviction
# admission reads status.disruptionsAllowed directly.

_nodecontroller_is_running_disruption() {
    if command -v systemctl >/dev/null 2>&1; then
        systemctl is-active --quiet nodecontroller 2>/dev/null && return 0
    fi
    pgrep -x nodecontroller >/dev/null 2>&1
}

_k3s_controller_manager_disabled_disruption() {
    local args=""
    if command -v systemctl >/dev/null 2>&1; then
        args="$(systemctl show k3s -p ExecStart --value 2>/dev/null || true)"
    fi
    [[ "$args" == *--disable-controller-manager* ]] && return 0
    ps -eo args= 2>/dev/null | grep -E '[k]3s( server)?' | grep -q -- '--disable-controller-manager'
}

_require_nodecontroller_disruption() {
    _nodecontroller_is_running_disruption \
        || skip_test "nodecontroller isn't running here — deploy with --controller-manager=nodecontroller to exercise these"
    _k3s_controller_manager_disabled_disruption \
        || skip_test "k3s's bundled controller-manager is still enabled; deploy with --controller-manager=nodecontroller so this test exercises nodecontroller"
}

test_disruption_controller_computes_pdb_status() {
    _require_nodecontroller_disruption
    local dep="pdb-test"
    local pdb="pdb-test-budget"

    apply_manifest <<EOF
apiVersion: apps/v1
kind: Deployment
metadata:
  name: $dep
spec:
  replicas: 3
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
    apply_manifest <<EOF
apiVersion: policy/v1
kind: PodDisruptionBudget
metadata:
  name: $pdb
spec:
  minAvailable: 2
  selector:
    matchLabels:
      app: $dep
EOF
    trap 'kctl delete pdb "$pdb" --ignore-not-found >/dev/null 2>&1 || true; kctl delete deployment "$dep" --ignore-not-found >/dev/null 2>&1 || true; kctl delete replicasets -l "app=$dep" --ignore-not-found >/dev/null 2>&1 || true; kctl delete pods -l "app=$dep" --ignore-not-found >/dev/null 2>&1 || true' EXIT

    wait_until 90 "deployment $dep has 3 ready Pods" \
        bash -c "[[ \"\$(kctl get deployment '$dep' -o jsonpath='{.status.readyReplicas}')\" == '3' ]]"

    wait_until 30 "PDB $pdb reports expectedPods=3" \
        bash -c "[[ \"\$(kctl get pdb '$pdb' -o jsonpath='{.status.expectedPods}')\" == '3' ]]"

    wait_until 30 "PDB $pdb reports currentHealthy=3" \
        bash -c "[[ \"\$(kctl get pdb '$pdb' -o jsonpath='{.status.currentHealthy}')\" == '3' ]]"

    wait_until 30 "PDB $pdb reports desiredHealthy=2 (minAvailable)" \
        bash -c "[[ \"\$(kctl get pdb '$pdb' -o jsonpath='{.status.desiredHealthy}')\" == '2' ]]"

    wait_until 30 "PDB $pdb reports disruptionsAllowed=1 (3 healthy - 2 desired)" \
        bash -c "[[ \"\$(kctl get pdb '$pdb' -o jsonpath='{.status.disruptionsAllowed}')\" == '1' ]]"

    trap - EXIT
    kctl delete pdb "$pdb" --ignore-not-found >/dev/null 2>&1 || true
    kctl delete deployment "$dep" --ignore-not-found >/dev/null 2>&1 || true
    kctl delete replicasets -l "app=$dep" --ignore-not-found >/dev/null 2>&1 || true
    kctl delete pods -l "app=$dep" --ignore-not-found >/dev/null 2>&1 || true
}

register_test test_disruption_controller_computes_pdb_status
