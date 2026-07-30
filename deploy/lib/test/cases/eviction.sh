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

register_test test_eviction_manual_procedure
