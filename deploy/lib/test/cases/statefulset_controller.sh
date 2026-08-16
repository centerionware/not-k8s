# lib/test/cases/statefulset_controller.sh — nodecontroller's Group E:
# statefulset-controller. Proves stable ordinal identity (pod-0, pod-1 by
# name, not a random suffix) and OrderedReady scale-up/scale-down semantics.

_nodecontroller_is_running_sts() {
    if command -v systemctl >/dev/null 2>&1; then
        systemctl is-active --quiet nodecontroller 2>/dev/null && return 0
    fi
    pgrep -x nodecontroller >/dev/null 2>&1
}

_k3s_controller_manager_disabled_sts() {
    local args=""
    if command -v systemctl >/dev/null 2>&1; then
        args="$(systemctl show k3s -p ExecStart --value 2>/dev/null || true)"
    fi
    [[ "$args" == *--disable-controller-manager* ]] && return 0
    ps -eo args= 2>/dev/null | grep -E '[k]3s( server)?' | grep -q -- '--disable-controller-manager'
}

_require_nodecontroller_sts() {
    _nodecontroller_is_running_sts \
        || skip_test "nodecontroller isn't running here — deploy with --controller-manager=nodecontroller to exercise these"
    _k3s_controller_manager_disabled_sts \
        || skip_test "k3s's bundled controller-manager is still enabled; deploy with --controller-manager=nodecontroller so this test exercises nodecontroller"
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
          command: ["sh", "-c", "while [ ! -f /tmp/release ]; do sleep 1; done; sleep 3600"]
          readinessProbe:
            exec:
              command: ["test", "-f", "/tmp/release"]
            periodSeconds: 1
EOF
    trap 'kctl delete statefulset "$sts" --ignore-not-found >/dev/null 2>&1 || true; kctl delete pods -l "app=$sts" --ignore-not-found >/dev/null 2>&1 || true' EXIT

    wait_until 60 "statefulset $sts creates pod ${sts}-0" \
        bash -c "kctl get pod '${sts}-0' >/dev/null 2>&1"

    # Keep ordinal zero Running but unready with a gate inside the container.
    # OrderedReady must not create ordinal one while this gate is closed.
    wait_until 30 "statefulset $sts starts pod ${sts}-0 but keeps it unready" \
        bash -c "[[ \"\$(kctl get pod '${sts}-0' -o jsonpath='{.status.phase}')\" == 'Running' && \"\$(kctl get pod '${sts}-0' -o jsonpath='{.status.conditions[?(@.type==\"Ready\")].status}')\" != 'True' ]]"
    sleep 5
    assert_true bash -c "! kctl get pod '${sts}-1' >/dev/null 2>&1" \
        "OrderedReady must not create ${sts}-1 while ${sts}-0 is unready"

    kctl exec "${sts}-0" -- touch /tmp/release >/dev/null

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

# CodeRabbit finding on PR #29: build_pod cloned the template PodSpec
# verbatim and never injected a Volume for each volumeClaimTemplate, so a
# container's volumeMount referencing one had no matching spec.volumes
# entry — the apiserver rejects that Pod outright (a real create-time
# 422, not a Pending state), regardless of whether any CSI provisioner
# exists to ever bind the PVC. Deliberately doesn't need a working
# provisioner (see csi_pvc.sh for that, separately gated): the fix under
# test is "the Pod object gets created and accepted at all," which is
# provable with no StorageClass, no external-provisioner, nothing but the
# stock local-path-provisioner every not-k8s node already has.
test_statefulset_with_a_volume_claim_template_creates_an_accepted_pod() {
    _require_nodecontroller_sts
    local sts="sts-pvc-test"

    apply_manifest <<EOF
apiVersion: apps/v1
kind: StatefulSet
metadata:
  name: $sts
spec:
  serviceName: $sts
  replicas: 1
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
          volumeMounts:
            - name: data
              mountPath: /data
  volumeClaimTemplates:
    - metadata:
        name: data
      spec:
        accessModes: ["ReadWriteOnce"]
        resources:
          requests:
            storage: 64Mi
EOF
    trap 'kctl delete statefulset "$sts" --ignore-not-found >/dev/null 2>&1 || true; kctl delete pods -l "app=$sts" --ignore-not-found >/dev/null 2>&1 || true; kctl delete pvc "data-${sts}-0" --ignore-not-found >/dev/null 2>&1 || true' EXIT

    # The bug this test exists for fails right here: without the fix, the
    # Pod object is never created at all (rejected by apiserver validation
    # on the volumeMount/volumes mismatch), so it never even appears.
    if ! try_wait_until 30 bash -c "kctl get pod '${sts}-0' >/dev/null 2>&1"; then
        die "the StatefulSet's Pod was never created at all — this is the exact symptom of a volumeMount with no matching spec.volumes entry; check nodecontroller's log for a Pod creation rejection"
    fi

    local vol_name claim_name
    vol_name="$(kctl get pod "${sts}-0" -o jsonpath='{.spec.volumes[?(@.persistentVolumeClaim)].name}')"
    claim_name="$(kctl get pod "${sts}-0" -o jsonpath='{.spec.volumes[?(@.persistentVolumeClaim)].persistentVolumeClaim.claimName}')"
    assert_eq "$vol_name" "data" "injected volume name must match the volumeMount/template name, not the generated PVC name"
    assert_eq "$claim_name" "data-${sts}-0" "injected volume's claimName must match the PVC build_pvc actually creates"

    trap - EXIT
    kctl delete statefulset "$sts" --ignore-not-found >/dev/null 2>&1 || true
    kctl delete pods -l "app=$sts" --ignore-not-found >/dev/null 2>&1 || true
    kctl delete pvc "data-${sts}-0" --ignore-not-found >/dev/null 2>&1 || true
}

register_test test_statefulset_creates_ordinal_pods_and_scales_down_highest_first
register_test test_statefulset_with_a_volume_claim_template_creates_an_accepted_pod
