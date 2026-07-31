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

register_test test_eviction_manual_procedure
register_test test_eviction_priority_tiebreak_manual_procedure
