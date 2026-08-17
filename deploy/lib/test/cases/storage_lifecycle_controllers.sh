# lib/test/cases/storage_lifecycle_controllers.sh — nodecontroller's Group
# G: persistentvolume-binder-controller and pv-protection-controller/
# pvc-protection-controller. These direct controller tests need no real CSI
# driver: one uses a hostPath static PV and the other verifies the dynamic
# provisioner handoff with a deliberately fake provisioner. csi_pvc.sh and
# csi_attach.sh separately exercise the complete handoff against real CSI.

_nodecontroller_is_running_storage() {
    if command -v systemctl >/dev/null 2>&1; then
        systemctl is-active --quiet nodecontroller 2>/dev/null && return 0
    fi
    pgrep -x nodecontroller >/dev/null 2>&1
}

_require_nodecontroller_storage() {
    _nodecontroller_is_running_storage \
        || skip_test "nodecontroller isn't running here — deploy with --controller-manager=nodecontroller (which also disables k3s's own controller manager) to exercise these"
}

test_pv_binder_binds_a_static_pv_and_protection_finalizers_gate_deletion() {
    _require_nodecontroller_storage
    local class="pv-binder-test-class"
    local pv="pv-binder-test"
    local pvc="pvc-binder-test"

    apply_manifest <<EOF
apiVersion: v1
kind: PersistentVolume
metadata:
  name: $pv
spec:
  capacity:
    storage: 10Mi
  accessModes: ["ReadWriteOnce"]
  storageClassName: $class
  hostPath:
    path: /tmp/not-k8s-e2e-$pv
EOF
    apply_manifest <<EOF
apiVersion: v1
kind: PersistentVolumeClaim
metadata:
  name: $pvc
spec:
  accessModes: ["ReadWriteOnce"]
  storageClassName: $class
  resources:
    requests:
      storage: 10Mi
EOF
    trap 'kctl delete pvc "$pvc" --ignore-not-found >/dev/null 2>&1 || true; kctl delete pv "$pv" --ignore-not-found >/dev/null 2>&1 || true' EXIT

    wait_until 30 "persistentvolume-binder-controller adds the pvc-protection finalizer" \
        bash -c "kctl get pvc '$pvc' -o jsonpath='{.metadata.finalizers}' | grep -q pvc-protection"

    wait_until 60 "persistentvolume-binder-controller binds $pvc to $pv (static path)" \
        bash -c "[[ \"\$(kctl get pvc '$pvc' -o jsonpath='{.spec.volumeName}')\" == '$pv' ]]"

    wait_until 30 "PVC $pvc reports Bound" \
        bash -c "[[ \"\$(kctl get pvc '$pvc' -o jsonpath='{.status.phase}')\" == 'Bound' ]]"

    wait_until 30 "PV $pv reports Bound" \
        bash -c "[[ \"\$(kctl get pv '$pv' -o jsonpath='{.status.phase}')\" == 'Bound' ]]"

    wait_until 30 "pv-protection-controller adds the pv-protection finalizer" \
        bash -c "kctl get pv '$pv' -o jsonpath='{.metadata.finalizers}' | grep -q pv-protection"

    # Deleting the PVC (unused by any Pod) should proceed immediately —
    # pvc-protection only blocks while a live Pod references it.
    kctl delete pvc "$pvc" --ignore-not-found >/dev/null 2>&1
    wait_until 30 "pvc-protection-controller releases $pvc once it's not referenced by any Pod" \
        bash -c "! kctl get pvc '$pvc' >/dev/null 2>&1"

    # Now that its PVC is gone, pv-protection-controller must release the
    # PV too — this is exactly the deadlock check: this controller never
    # flips PV status back to Released, so protection must key off the
    # claim actually existing, not status.phase, or this would hang forever.
    kctl delete pv "$pv" --ignore-not-found >/dev/null 2>&1
    wait_until 30 "pv-protection-controller releases $pv once its PVC is gone" \
        bash -c "! kctl get pv '$pv' >/dev/null 2>&1"

    trap - EXIT
}

register_test test_pv_binder_binds_a_static_pv_and_protection_finalizers_gate_deletion

# Upstream's PV controller, not the external provisioner itself, turns an
# unbound StorageClass-backed claim into provisioning work by copying the
# class's provisioner onto this annotation. Without it a perfectly healthy
# CSI provisioner watches the PVC but deliberately ignores it forever. This
# uses a fake provisioner so it proves the handoff directly without requiring
# the optional reference CSI driver.
test_pv_binder_requests_dynamic_provisioning_from_the_storage_class() {
    _require_nodecontroller_storage
    local class="pv-binder-dynamic-handoff-class"
    local pvc="pv-binder-dynamic-handoff-claim"
    local provisioner="not-k8s.test/fake-provisioner"

    apply_manifest <<EOF
apiVersion: storage.k8s.io/v1
kind: StorageClass
metadata:
  name: $class
provisioner: $provisioner
volumeBindingMode: Immediate
EOF
    apply_manifest <<EOF
apiVersion: v1
kind: PersistentVolumeClaim
metadata:
  name: $pvc
spec:
  accessModes: ["ReadWriteOnce"]
  storageClassName: $class
  resources:
    requests:
      storage: 10Mi
EOF
    trap 'kctl delete pvc "$pvc" --ignore-not-found >/dev/null 2>&1 || true; kubectl delete storageclass "$class" --ignore-not-found >/dev/null 2>&1 || true' EXIT

    wait_until 30 "persistentvolume-binder-controller requests the StorageClass provisioner" \
        bash -c "[[ \"\$(kctl get pvc '$pvc' -o jsonpath='{.metadata.annotations.volume\\.kubernetes\\.io/storage-provisioner}')\" == '$provisioner' ]]"

    kctl delete pvc "$pvc" --ignore-not-found >/dev/null 2>&1 || true
    kubectl delete storageclass "$class" --ignore-not-found >/dev/null 2>&1 || true
    trap - EXIT
}

register_test test_pv_binder_requests_dynamic_provisioning_from_the_storage_class
