# lib/test/cases/eviction.sh — node-pressure eviction (eviction.rs). Real
# eviction only fires under genuine MemoryPressure/DiskPressure/PIDPressure
# — deliberately exhausting any of those on a real node this suite is
# running on is exactly the kind of destructive action worth not doing
# automatically. Pure ranking logic (QoS class, critical-pod protection) is
# already covered by `cargo test` (crates/nodelet/src/eviction_tests/); this
# documents the manual live-cluster procedure instead of faking pressure.

test_eviction_manual_procedure() {
    skip_test "triggering real eviction safely needs either an artificially-low pressure threshold or actually exhausting a resource — neither is something this suite does to a live node automatically. Manual procedure: (1) note this node's available memory, (2) restart nodelet with NODELET_MEMORY_PRESSURE_THRESHOLD_BYTES set just above current MemAvailable (forces MemoryPressure=True without real exhaustion), (3) apply a BestEffort pod and a Guaranteed pod, (4) within NODELET_EVICTION_CHECK_SECS (default 10s) confirm the BestEffort pod gets phase Failed/reason Evicted and is deleted, while the Guaranteed pod is untouched, (5) restore the normal threshold and restart nodelet."
}

test_eviction_priority_tiebreak_manual_procedure() {
    skip_test "proving priority-based tiebreaking (round 26 — pick_eviction_candidate() ranks by spec.priority within a QoS class before falling back to usage) also needs artificial pressure, same limitation as the base eviction procedure above. Manual procedure: create two BestEffort pods with different PriorityClasses (e.g. a low-priority one and a high-priority one, or set spec.priority directly), force MemoryPressure the same way test_eviction_manual_procedure describes, and confirm the LOWER-priority pod is evicted first even if the higher-priority pod is using more memory — proof priority wins the tiebreak before usage does, matching real kubelet's rankMemoryPressure ordering. The pure ranking logic itself (priority beats usage, QoS class still outranks priority) is already covered by cargo test's eviction_tests/pick_candidate.rs — this only proves the live wiring end to end."
}

test_pod_exceeding_its_own_ephemeral_storage_limit_is_evicted() {
    # Round 49: unlike the node-level pressure eviction above, a pod
    # exceeding its OWN ephemeral-storage limit is a direct per-pod
    # resource violation — checked independently of MemoryPressure/
    # DiskPressure/PIDPressure, so this is genuinely automatable without
    # faking node-wide pressure. Writes well past the 1Mi limit into a
    # plain (disk-backed) emptyDir, which nodelet materializes at
    # VOLUME_ROOT/<uid>/volumes/data — exactly the directory
    # directory_usage_bytes() walks.
    if ! node_uses_cri_runtime; then skip_test "needs cri runtime"; fi
    local name="ephemeral-storage-limit-check"
    apply_manifest <<EOF
apiVersion: v1
kind: Pod
metadata:
  name: $name
spec:
  containers:
    - name: app
      image: $TEST_IMAGE
      resources:
        limits:
          ephemeral-storage: "1Mi"
      command: ["sh", "-c", "dd if=/dev/zero of=/data/bigfile bs=1M count=8 2>/dev/null; sleep 3600"]
      volumeMounts:
        - name: data
          mountPath: /data
  volumes:
    - name: data
      emptyDir: {}
EOF
    wait_until 30 "$name Running" pod_is_phase "$name" Running
    wait_until 60 "$name evicted for exceeding its own ephemeral-storage limit" bash -c \
        "[[ \"\$(kctl get pod '$name' -o jsonpath='{.status.reason}')\" == 'Evicted' ]]"
    delete_pod_if_exists "$name"
}

test_pod_exceeding_an_empty_dir_size_limit_is_evicted() {
    # Round 67: distinct from the whole-pod ephemeral-storage limit above
    # — this pod has no ephemeral-storage limit at all, only a per-volume
    # emptyDir.sizeLimit, and that alone must trigger eviction. Scoped to
    # plain-disk emptyDir only (a Memory/HugePages-medium emptyDir's
    # sizeLimit is already a real kernel-enforced cap at mount time,
    # rounds 30/61 — see empty_dir_size_limits()'s own pure-logic tests
    # for that scoping).
    if ! node_uses_cri_runtime; then skip_test "needs cri runtime"; fi
    local name="empty-dir-size-limit-check"
    apply_manifest <<EOF
apiVersion: v1
kind: Pod
metadata:
  name: $name
spec:
  containers:
    - name: app
      image: $TEST_IMAGE
      command: ["sh", "-c", "dd if=/dev/zero of=/data/bigfile bs=1M count=8 2>/dev/null; sleep 3600"]
      volumeMounts:
        - name: data
          mountPath: /data
  volumes:
    - name: data
      emptyDir:
        sizeLimit: 1Mi
EOF
    wait_until 30 "$name Running" pod_is_phase "$name" Running
    wait_until 60 "$name evicted for exceeding its emptyDir volume's sizeLimit" bash -c \
        "[[ \"\$(kctl get pod '$name' -o jsonpath='{.status.reason}')\" == 'Evicted' ]]"
    delete_pod_if_exists "$name"
}

register_test test_eviction_manual_procedure
register_test test_eviction_priority_tiebreak_manual_procedure
register_test test_pod_exceeding_its_own_ephemeral_storage_limit_is_evicted
register_test test_pod_exceeding_an_empty_dir_size_limit_is_evicted
