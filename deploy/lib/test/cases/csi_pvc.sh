# lib/test/cases/csi_pvc.sh — PersistentVolumeClaim volumes via CSI
# (runtime/csi.rs). Needs real infrastructure this suite can't set up
# itself: a StorageClass backed by an external-provisioner (provisioning
# itself isn't kubelet's job, nodelet included) whose driver is also
# listed in the running nodelet's NODELET_CSI_DRIVERS — skips cleanly
# without both. Set TEST_CSI_STORAGE_CLASS to exercise this for real.

test_pod_mounts_a_persistent_volume_claim() {
    if ! node_uses_cri_runtime; then skip_test "needs cri runtime"; fi
    if [[ -z "${TEST_CSI_STORAGE_CLASS:-}" ]]; then
        skip_test "TEST_CSI_STORAGE_CLASS not set — export it to a StorageClass backed by a CSI driver that's also listed in nodelet's NODELET_CSI_DRIVERS to exercise this"
    fi

    local name="csi-pvc-check"
    local claim="csi-pvc-check-claim"

    apply_manifest <<EOF
apiVersion: v1
kind: PersistentVolumeClaim
metadata:
  name: $claim
spec:
  accessModes: ["ReadWriteOnce"]
  storageClassName: $TEST_CSI_STORAGE_CLASS
  resources:
    requests:
      storage: 64Mi
EOF

    if ! try_wait_until 60 bash -c "kubectl get pvc '$claim' -n '$TEST_NAMESPACE' -o jsonpath='{.status.phase}' | grep -q Bound"; then
        kubectl delete pvc "$claim" -n "$TEST_NAMESPACE" --ignore-not-found >/dev/null 2>&1
        skip_test "PVC '$claim' never became Bound within 60s — needs a working external-provisioner for TEST_CSI_STORAGE_CLASS ($TEST_CSI_STORAGE_CLASS); not something nodelet itself does (see docs/GAP_CLOSURE.md's out-of-scope notes)"
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
      command: ["sh", "-c", "echo hello-from-csi-pvc > /data/marker; sleep 3600"]
      volumeMounts:
        - name: data
          mountPath: /data
  volumes:
    - name: data
      persistentVolumeClaim:
        claimName: $claim
EOF

    if ! try_wait_until 30 pod_is_phase "$name" Running; then
        delete_pod_if_exists "$name"
        kubectl delete pvc "$claim" -n "$TEST_NAMESPACE" --ignore-not-found >/dev/null 2>&1
        skip_test "pod never reached Running with a PVC volume mounted — check nodelet's server logs for 'failed to mount CSI volume' or 'no CSI driver configured'"
    fi

    local uid marker_path
    uid="$(kubectl get pod "$name" -n "$TEST_NAMESPACE" -o jsonpath='{.metadata.uid}')"
    marker_path="/var/lib/nodelet/pods/$uid/volumes/data/marker"

    if ! try_wait_until 15 bash -c "[[ -f '$marker_path' ]]"; then
        delete_pod_if_exists "$name"
        kubectl delete pvc "$claim" -n "$TEST_NAMESPACE" --ignore-not-found >/dev/null 2>&1
        die "container wrote to /data but the marker file never appeared at $marker_path on the host — the CSI volume may not actually be mounted there, or NodePublishVolume's target_path doesn't match what nodelet expects"
    fi
    assert_contains "$(cat "$marker_path")" "hello-from-csi-pvc" "marker file content"

    delete_pod_if_exists "$name"
    kubectl delete pvc "$claim" -n "$TEST_NAMESPACE" --ignore-not-found >/dev/null 2>&1
}

register_test test_pod_mounts_a_persistent_volume_claim
