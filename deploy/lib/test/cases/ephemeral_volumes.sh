# lib/test/cases/ephemeral_volumes.sh — generic ephemeral volumes
# (spec.volumes[].ephemeral, round 31). Needs the same real infrastructure
# csi_pvc.sh does (a StorageClass backed by a working external-provisioner
# and a CSI driver listed in nodelet's NODELET_CSI_DRIVERS) *plus* a
# cluster whose kube-controller-manager actually runs the ephemeral-volume
# controller (creating the PVC from the pod's inline template) — nodelet
# itself never creates that PVC, only reads it once it exists. Set
# TEST_CSI_STORAGE_CLASS to exercise this for real.

_require_nodecontroller_ephemeral() {
    test_component_running nodecontroller \
        || skip_test "nodecontroller isn't running here — deploy with --controller-manager=nodecontroller to exercise generic ephemeral volumes"
    test_controller_manager_is_exclusive \
        || skip_test "k3s's bundled controller-manager is still enabled; this test would not prove nodecontroller's ephemeral-volume controller"
}

test_pod_mounts_a_generic_ephemeral_volume() {
    _require_nodecontroller_ephemeral
    if ! node_uses_cri_runtime; then skip_test "needs cri runtime"; fi
    if [[ -z "${TEST_CSI_STORAGE_CLASS:-}" ]]; then
        skip_test "TEST_CSI_STORAGE_CLASS not set — export it to a StorageClass backed by a CSI driver that's also listed in nodelet's NODELET_CSI_DRIVERS to exercise this"
    fi

    local name="ephemeral-vol-check"
    local expected_claim="${name}-data"

    # Unlike csi_pvc.sh, there's no PVC to pre-create — the whole point is
    # that the ephemeral-volume controller creates one automatically, named
    # "<pod name>-<volume name>", once it sees this pod.
    apply_manifest <<EOF
apiVersion: v1
kind: Pod
metadata:
  name: $name
spec:
  containers:
    - name: app
      image: $TEST_IMAGE
      command: ["sh", "-c", "echo hello-from-ephemeral-vol > /data/marker; sleep 3600"]
      volumeMounts:
        - name: data
          mountPath: /data
  volumes:
    - name: data
      ephemeral:
        volumeClaimTemplate:
          metadata:
            labels:
              not-k8s-e2e: generic-ephemeral
          spec:
            accessModes: ["ReadWriteOnce"]
            storageClassName: $TEST_CSI_STORAGE_CLASS
            resources:
              requests:
                storage: 64Mi
EOF
    trap 'delete_pod_and_pvc "$name" "$expected_claim"' EXIT

    if ! try_wait_until 90 bash -c "kubectl get pvc '$expected_claim' -n '$TEST_NAMESPACE' >/dev/null 2>&1"; then
        delete_pod_if_exists "$name"
        die "PersistentVolumeClaim '$expected_claim' was never created — nodecontroller's ephemeral-volume-controller did not materialize the Pod's volumeClaimTemplate"
    fi

    local pod_uid owner_uid
    pod_uid="$(kubectl get pod "$name" -n "$TEST_NAMESPACE" -o jsonpath='{.metadata.uid}')"
    owner_uid="$(kubectl get pvc "$expected_claim" -n "$TEST_NAMESPACE" -o jsonpath='{.metadata.ownerReferences[?(@.controller==true)].uid}')"
    assert_eq "$owner_uid" "$pod_uid" "generic ephemeral PVC controller owner UID"
    assert_eq "$(kubectl get pvc "$expected_claim" -n "$TEST_NAMESPACE" -o jsonpath='{.metadata.labels.not-k8s-e2e}')" "generic-ephemeral" "generic ephemeral PVC copied template labels"

    if ! try_wait_until 90 bash -c "kubectl get pvc '$expected_claim' -n '$TEST_NAMESPACE' -o jsonpath='{.status.phase}' | grep -q Bound"; then
        delete_pod_and_pvc "$name" "$expected_claim"
        die "PVC '$expected_claim' never became Bound within 60s — needs a working external-provisioner for TEST_CSI_STORAGE_CLASS ($TEST_CSI_STORAGE_CLASS)"
    fi

    # Round 124 (found live in CI, full-suite runs only): nodelet's own log
    # confirmed a real attach race under load — "driver requires attach but
    # no matching VolumeAttachment exists yet (external-attacher hasn't
    # created it); will retry next reconcile" — not a mount bug, just attach
    # latency from the whole unfiltered suite sharing one CSI
    # driver/attacher. 30s was tight enough to fail across two separate
    # full-pipeline runs despite passing reliably in smaller batches.
    if ! try_wait_until 90 pod_is_phase "$name" Running; then
        delete_pod_and_pvc "$name" "$expected_claim"
        die "pod never reached Running with the ephemeral volume mounted — check nodelet's logs for 'failed to mount CSI volume for generic ephemeral volume' or 'isn't owned by this pod'"
    fi

    local uid marker_path
    uid="$(kubectl get pod "$name" -n "$TEST_NAMESPACE" -o jsonpath='{.metadata.uid}')"
    marker_path="/var/lib/nodelet/pods/$uid/volumes/data/marker"

    if ! try_wait_until 30 bash -c "[[ -f '$marker_path' ]]"; then
        delete_pod_and_pvc "$name" "$expected_claim"
        die "container wrote to /data but the marker file never appeared at $marker_path on the host — check ephemeral_pvc_name()/resolve_ephemeral_source() in runtime/cri.rs"
    fi
    assert_contains "$(cat "$marker_path")" "hello-from-ephemeral-vol" "marker file content"

    trap - EXIT
    delete_pod_and_pvc "$name" "$expected_claim"
}

register_test test_pod_mounts_a_generic_ephemeral_volume csi_dra
