# lib/test/cases/csi_attach.sh — CSI attach coordination (runtime/cri.rs's
# resolve_csi_source(): CSIDriver.spec.attachRequired + waiting on a
# VolumeAttachment's status.attached before Stage/Publish). Needs a
# StorageClass backed by a CSI driver that actually requires attach (most
# node-local/edge drivers set attachRequired: false and never exercise this
# path at all — see csi_pvc.sh for that case) *and* a working
# external-attacher sidecar creating/updating VolumeAttachment objects for
# it — set TEST_CSI_ATTACH_STORAGE_CLASS to exercise this for real.

test_pod_with_an_attach_required_pvc_waits_for_volumeattachment() {
    if ! node_uses_cri_runtime; then skip_test "needs cri runtime"; fi
    if [[ -z "${TEST_CSI_ATTACH_STORAGE_CLASS:-}" ]]; then
        skip_test "TEST_CSI_ATTACH_STORAGE_CLASS not set — export it to a StorageClass backed by an attachRequired CSI driver (with a working external-attacher) to exercise this"
    fi

    local name="csi-attach-check"
    local claim="csi-attach-check-claim"

    apply_manifest <<EOF
apiVersion: v1
kind: PersistentVolumeClaim
metadata:
  name: $claim
spec:
  accessModes: ["ReadWriteOnce"]
  storageClassName: $TEST_CSI_ATTACH_STORAGE_CLASS
  resources:
    requests:
      storage: 64Mi
EOF

    if ! try_wait_until 90 bash -c "kubectl get pvc '$claim' -n '$TEST_NAMESPACE' -o jsonpath='{.status.phase}' | grep -q Bound"; then
        kubectl delete pvc "$claim" -n "$TEST_NAMESPACE" --ignore-not-found >/dev/null 2>&1
        skip_test "PVC '$claim' never became Bound within 60s — needs a working external-provisioner for TEST_CSI_ATTACH_STORAGE_CLASS"
    fi

    apply_manifest <<EOF
apiVersion: v1
kind: Pod
metadata:
  name: $name
spec:
  containers:
    - name: app
      image: $TEST_IMAGE
      command: ["sleep", "3600"]
      volumeMounts:
        - name: data
          mountPath: /data
  volumes:
    - name: data
      persistentVolumeClaim:
        claimName: $claim
EOF

    if ! try_wait_until 120 pod_is_phase "$name" Running; then
        delete_pod_and_pvc "$name" "$claim"
        die "pod never reached Running — check nodelet's logs for 'driver requires attach but no matching VolumeAttachment exists yet' (external-attacher not running) or 'VolumeAttachment found but not yet attached' (attach still in progress)"
    fi

    local pv_name attached
    pv_name="$(kubectl get pvc "$claim" -n "$TEST_NAMESPACE" -o jsonpath='{.spec.volumeName}')"
    attached="$(kubectl get volumeattachments.storage.k8s.io -o jsonpath="{.items[?(@.spec.source.persistentVolumeName=='$pv_name')].status.attached}")"
    assert_eq "$attached" "true" "the pod only reached Running because nodelet found this PersistentVolume's VolumeAttachment already attached"

    delete_pod_and_pvc "$name" "$claim"
}

register_test test_pod_with_an_attach_required_pvc_waits_for_volumeattachment csi_dra
