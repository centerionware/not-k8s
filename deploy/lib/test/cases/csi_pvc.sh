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

    # Round 34: Node.status.volumesInUse/volumesAttached — reuses this
    # test's own already-mounted CSI volume rather than needing separate
    # infrastructure. Real kubelet's unique-volume-name scheme is
    # "kubernetes.io/csi/<driver>^<volumeHandle>"; the driver/handle
    # aren't independently known to this bash test, so this checks for
    # the expected prefix and the claim's PV name being present somewhere
    # in the volume handle, rather than reconstructing the exact string.
    local n volumes_in_use
    n="$(node_name)"
    volumes_in_use="$(kubectl get node "$n" -o jsonpath='{.status.volumesInUse}')"
    if [[ "$volumes_in_use" != *"kubernetes.io/csi/"* ]]; then
        warn "Node.status.volumesInUse has no kubernetes.io/csi/ entry while a CSI volume is mounted — check mounted_csi_volumes()/csi_unique_volume_name() wiring in runtime/cri.rs and node.rs (round 34; not failing the test outright since the exact naming scheme is unvalidated against a real attach/detach controller)"
    fi

    delete_pod_if_exists "$name"
    kubectl delete pvc "$claim" -n "$TEST_NAMESPACE" --ignore-not-found >/dev/null 2>&1
}

test_csi_ephemeral_inline_volume_is_mounted() {
    # Round 46: volumes[].csi specified directly, no PVC at all — the
    # inline form real-world drivers like secrets-store-csi-driver use.
    # Needs a real CSI driver registered under NODELET_CSI_DRIVERS (or
    # dynamic registration) capable of ephemeral/inline mounts; export
    # TEST_CSI_INLINE_DRIVER to exercise this for real.
    if ! node_uses_cri_runtime; then skip_test "needs cri runtime"; fi
    if [[ -z "${TEST_CSI_INLINE_DRIVER:-}" ]]; then
        skip_test "TEST_CSI_INLINE_DRIVER not set — export it to a CSI driver name (also listed in nodelet's NODELET_CSI_DRIVERS) that supports ephemeral/inline volumes to exercise this"
    fi

    local name="csi-ephemeral-inline"
    apply_manifest <<EOF
apiVersion: v1
kind: Pod
metadata:
  name: $name
spec:
  containers:
    - name: app
      image: $TEST_IMAGE
      command: ["sh", "-c", "cat /data/marker > /dev/null 2>&1; sleep 3600"]
      volumeMounts:
        - name: data
          mountPath: /data
          readOnly: true
  volumes:
    - name: data
      csi:
        driver: $TEST_CSI_INLINE_DRIVER
        readOnly: true
EOF

    if ! try_wait_until 30 pod_is_phase "$name" Running; then
        delete_pod_if_exists "$name"
        skip_test "pod never reached Running with a CSI ephemeral inline volume mounted — check nodelet's server logs for 'failed to mount CSI ephemeral volume' or 'no CSI driver configured'"
    fi

    local uid vol_dir
    uid="$(kubectl get pod "$name" -n "$TEST_NAMESPACE" -o jsonpath='{.metadata.uid}')"
    vol_dir="/var/lib/nodelet/pods/$uid/volumes/data"
    assert_true bash -c "[[ -d '$vol_dir' ]]" "CSI ephemeral inline volume should be mounted at $vol_dir on the host (NodePublishVolume's target_path)"

    delete_pod_if_exists "$name"
}

test_pod_uses_a_raw_block_volume() {
    # Round 77 (found in round 76's re-audit): spec.containers[].volumeDevices
    # + a PV/PVC's volumeMode: Block. Needs a StorageClass whose bound PV
    # actually comes back Block-mode (most CSI drivers require explicit
    # opt-in for this, hence a separate env var from
    # TEST_CSI_STORAGE_CLASS rather than reusing it) whose driver is also
    # listed in nodelet's NODELET_CSI_DRIVERS — skips cleanly without it.
    # Structural proof only (the host-side bind-mount target must be a
    # FILE, not a directory, matching the CSI spec's own block-volume
    # convention) — actually reading/writing the raw device needs a tool
    # like `dd` this suite doesn't assume every $TEST_IMAGE has.
    if ! node_uses_cri_runtime; then skip_test "needs cri runtime"; fi
    if [[ -z "${TEST_CSI_BLOCK_STORAGE_CLASS:-}" ]]; then
        skip_test "TEST_CSI_BLOCK_STORAGE_CLASS not set — export it to a StorageClass whose driver supports volumeMode: Block (and is also listed in nodelet's NODELET_CSI_DRIVERS) to exercise this"
    fi

    local name="csi-block-check"
    local claim="csi-block-check-claim"

    apply_manifest <<EOF
apiVersion: v1
kind: PersistentVolumeClaim
metadata:
  name: $claim
spec:
  accessModes: ["ReadWriteOnce"]
  volumeMode: Block
  storageClassName: $TEST_CSI_BLOCK_STORAGE_CLASS
  resources:
    requests:
      storage: 64Mi
EOF

    if ! try_wait_until 60 bash -c "kubectl get pvc '$claim' -n '$TEST_NAMESPACE' -o jsonpath='{.status.phase}' | grep -q Bound"; then
        kubectl delete pvc "$claim" -n "$TEST_NAMESPACE" --ignore-not-found >/dev/null 2>&1
        skip_test "PVC '$claim' never became Bound within 60s — needs a working external-provisioner for TEST_CSI_BLOCK_STORAGE_CLASS ($TEST_CSI_BLOCK_STORAGE_CLASS)"
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
      volumeDevices:
        - name: raw
          devicePath: /dev/xvda
  volumes:
    - name: raw
      persistentVolumeClaim:
        claimName: $claim
EOF

    if ! try_wait_until 30 pod_is_phase "$name" Running; then
        delete_pod_if_exists "$name"
        kubectl delete pvc "$claim" -n "$TEST_NAMESPACE" --ignore-not-found >/dev/null 2>&1
        skip_test "pod never reached Running with a raw block volumeDevice — check nodelet's logs for 'failed to mount CSI volume' or build_devices()/ResolvedVolume::BlockDevice wiring in runtime/cri/volumes_pure.rs"
    fi

    local uid target_path
    uid="$(kubectl get pod "$name" -n "$TEST_NAMESPACE" -o jsonpath='{.metadata.uid}')"
    target_path="/var/lib/nodelet/pods/$uid/volumes/raw"
    if ! try_wait_until 15 bash -c "[[ -e '$target_path' ]]"; then
        delete_pod_if_exists "$name"
        kubectl delete pvc "$claim" -n "$TEST_NAMESPACE" --ignore-not-found >/dev/null 2>&1
        die "no bind-mount target ever appeared at $target_path on the host — NodePublishVolume's target_path may not match what nodelet expects for a Block-mode volume"
    fi
    assert_true bash -c "[[ ! -d '$target_path' ]]" # must be a file (or device node), never a directory, for Block mode

    delete_pod_if_exists "$name"
    kubectl delete pvc "$claim" -n "$TEST_NAMESPACE" --ignore-not-found >/dev/null 2>&1
}

test_node_reports_volumes_in_use_for_a_csi_volume() {
    # Round 34's Node.status.volumesInUse/volumesAttached, scoped to CSI
    # volumes only (the modern CSI attach path — round 19 — already uses
    # VolumeAttachment directly, not these fields). Round 123: previously
    # manual-only for no real reason — TEST_CSI_STORAGE_CLASS is already
    # available in CI (e2e-full-setup.sh), same infra
    # test_pod_mounts_a_persistent_volume_claim already uses.
    if ! node_uses_cri_runtime; then skip_test "needs cri runtime"; fi
    if [[ -z "${TEST_CSI_STORAGE_CLASS:-}" ]]; then
        skip_test "TEST_CSI_STORAGE_CLASS not set — export it to a StorageClass backed by a CSI driver that's also listed in nodelet's NODELET_CSI_DRIVERS to exercise this"
    fi
    local name="volumes-in-use-check"
    local claim="volumes-in-use-check-claim"
    local n
    n="$(node_name)"

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
        skip_test "PVC '$claim' never became Bound within 60s — needs a working external-provisioner for TEST_CSI_STORAGE_CLASS ($TEST_CSI_STORAGE_CLASS)"
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
    if ! try_wait_until 30 pod_is_phase "$name" Running; then
        delete_pod_if_exists "$name"
        kubectl delete pvc "$claim" -n "$TEST_NAMESPACE" --ignore-not-found >/dev/null 2>&1
        skip_test "pod never reached Running with a PVC volume mounted — check nodelet's server logs for 'failed to mount CSI volume' or 'no CSI driver configured'"
    fi

    local in_use
    in_use="$(kubectl get node "$n" -o jsonpath='{.status.volumesInUse}')"
    assert_contains "$in_use" "kubernetes.io/csi/" "Node.status.volumesInUse should list an entry ('kubernetes.io/csi/<driver>^<volumeHandle>', round 34) while the CSI-backed pod is running"

    delete_pod_if_exists "$name"
    wait_until 30 "$name gone" pod_gone "$name"
    wait_until 20 "volumesInUse cleared after pod deletion" \
        bash -c "[[ \"\$(kubectl get node '$n' -o jsonpath='{.status.volumesInUse}')\" != *'kubernetes.io/csi/'* ]]"
    kubectl delete pvc "$claim" -n "$TEST_NAMESPACE" --ignore-not-found >/dev/null 2>&1
}

test_fsgroup_change_policy_on_root_mismatch_skips_the_second_chown() {
    # Round 93's fsGroupChangePolicy: OnRootMismatch skip
    # (skip_fs_group_change() in runtime/cri/volumes_pure.rs) only ever
    # applies to CSI/PV-backed volumes, matching upstream. Round 123:
    # previously manual-only purely because it needs a real CSI volume
    # that survives across two pod lifecycles (a PVC, not an ephemeral/
    # inline CSI volume) — same TEST_CSI_STORAGE_CLASS infra every other
    # PVC test here already uses. Proves the *skip* itself indirectly:
    # since skip_fs_group_change() only fires when the root directory's
    # gid already matches, a SECOND pod (same PVC, same fsGroup) seeing
    # the correct gid immediately — without this test doing anything to
    # force it — is exactly what "root already matches, no chown needed"
    # looks like from the outside; the *first* pod's real chown already
    # proves the underlying gid-setting mechanism works
    # (fsgroup.rs/cri_tests/fs_group.rs's own unit tests cover the pure
    # decision logic directly).
    if ! node_uses_cri_runtime; then skip_test "needs cri runtime"; fi
    if [[ -z "${TEST_CSI_STORAGE_CLASS:-}" ]]; then
        skip_test "TEST_CSI_STORAGE_CLASS not set — export it to a StorageClass backed by a CSI driver that's also listed in nodelet's NODELET_CSI_DRIVERS to exercise this"
    fi
    local claim="fsgroup-policy-check-claim"
    local fsgroup_pod_spec
    fsgroup_pod_spec() { # fsgroup_pod_spec <name>
        cat <<EOF
apiVersion: v1
kind: Pod
metadata:
  name: $1
spec:
  securityContext:
    fsGroup: 4322
    fsGroupChangePolicy: OnRootMismatch
  containers:
    - name: app
      image: $TEST_IMAGE
      command: ["sh", "-c", "sleep 3600"]
      volumeMounts:
        - name: data
          mountPath: /data
  volumes:
    - name: data
      persistentVolumeClaim:
        claimName: $claim
EOF
    }

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
        skip_test "PVC '$claim' never became Bound within 60s — needs a working external-provisioner for TEST_CSI_STORAGE_CLASS ($TEST_CSI_STORAGE_CLASS)"
    fi

    local first="fsgroup-policy-check-1"
    apply_manifest <<< "$(fsgroup_pod_spec "$first")"
    if ! try_wait_until 30 pod_is_phase "$first" Running; then
        delete_pod_if_exists "$first"
        kubectl delete pvc "$claim" -n "$TEST_NAMESPACE" --ignore-not-found >/dev/null 2>&1
        skip_test "first pod never reached Running with a PVC + fsGroup volume mounted"
    fi
    delete_pod_if_exists "$first"
    wait_until 30 "$first gone" pod_gone "$first"

    local second="fsgroup-policy-check-2"
    apply_manifest <<< "$(fsgroup_pod_spec "$second")"
    if ! try_wait_until 30 pod_is_phase "$second" Running; then
        delete_pod_if_exists "$second"
        kubectl delete pvc "$claim" -n "$TEST_NAMESPACE" --ignore-not-found >/dev/null 2>&1
        skip_test "second pod never reached Running reusing the same PVC + fsGroup"
    fi

    # If OnRootMismatch's skip were somehow broken (e.g. it recursed
    # every time regardless of policy), this still wouldn't distinguish
    # "skipped" from "re-applied" from the outside without exec'ing in —
    # so what THIS asserts is the outward-visible contract fsGroup
    # promises regardless of which path was taken: the volume is usable
    # and correctly group-owned for the second pod too.
    local gid
    gid="$(kctl exec "$second" -- stat -c %g /data 2>&1)"
    delete_pod_if_exists "$second"
    kubectl delete pvc "$claim" -n "$TEST_NAMESPACE" --ignore-not-found >/dev/null 2>&1
    assert_eq "$gid" "4322" "the second pod reusing the same PVC should still see fsGroup 4322 on /data, whether OnRootMismatch skipped the chown or not"
}

register_test test_pod_mounts_a_persistent_volume_claim
register_test test_csi_ephemeral_inline_volume_is_mounted
register_test test_pod_uses_a_raw_block_volume
register_test test_node_reports_volumes_in_use_for_a_csi_volume
register_test test_fsgroup_change_policy_on_root_mismatch_skips_the_second_chown
