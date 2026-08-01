//! Memory Manager: real kubelet's `--memory-manager-policy=Static` — NUMA
//! node pinning (`cpuset.mems`) for Guaranteed-QoS containers with a set
//! memory limit, the last of the three upstream managers alongside CPU
//! Manager (`cpu_manager.rs`, rounds 15-16) and Topology Manager
//! (`topology.rs`, round 17), which this becomes a third hint provider
//! for. Real value: a Guaranteed pod's memory now actually lives on a
//! specific NUMA node instead of wherever the kernel's own NUMA balancer
//! happens to place it, which matters once its CPUs are pinned too —
//! cross-node memory access is the exact latency CPU pinning alone
//! doesn't avoid.
//!
//! Opt-in, matching upstream: `NODELET_MEMORY_MANAGER_POLICY` defaults to
//! `none` — `static` turns this on.
//!
//! **Scoped as a first slice, several real simplifications documented
//! here rather than silently thin**:
//! - **Single-NUMA-node only, never spans.** Real Memory Manager can pin
//!   a container's memory across *multiple* NUMA nodes if no single node
//!   has enough free capacity. This implementation only ever pins to one
//!   node — if none has enough room, the container falls back to
//!   unconstrained (`cpuset_mems` left unset), the same graceful-
//!   degradation choice `cpu_manager.rs` makes for CPU.
//! - **No shared-pool tracking for non-pinned containers.** CPU Manager
//!   (rounds 15-16) explicitly sets *every* container's `cpuset_cpus` to
//!   the current shared pool and retroactively updates already-running
//!   ones as claims change. Memory Manager here only ever sets
//!   `cpuset_mems` on containers it actually pins — everything else is
//!   left unconstrained (able to use any NUMA node's memory), which is
//!   memory-safe, just less strictly isolated than upstream's own
//!   accounting. No `UpdateContainerResources` retroactive sweep exists
//!   for memory.
//! - **No per-NUMA-node reservation.** Real Memory Manager supports a
//!   `--reserved-memory` flag reserving specific bytes per node for
//!   system/kube use, mirroring `--reserved-cpus`. This implementation
//!   tracks total NUMA node capacity only — `system-reserved`/
//!   `kube-reserved` memory (already subtracted from `Node.status.allocatable`,
//!   see `node.rs`) isn't additionally carved out of any specific node here.
//! - **Per-container, not per-pod.** Same simplification `cpu_manager.rs`
//!   already makes: real kubelet aggregates a Guaranteed pod's *every*
//!   container's memory into one admission decision; this pins each
//!   qualifying container independently.

use crate::eviction::QosClass;
use std::collections::{BTreeMap, HashMap};
use std::sync::Mutex;

/// Whether a container's QoS class + memory limit make it eligible for
/// NUMA memory pinning — Guaranteed QoS with a positive memory limit set.
/// Unlike CPU Manager's "whole number of cores" rule, memory has no
/// analogous "integer" requirement — any positive limit qualifies.
pub fn wants_pinned_memory(qos: QosClass, memory_limit_bytes: Option<i64>) -> Option<u64> {
    if qos != QosClass::Guaranteed {
        return None;
    }
    match memory_limit_bytes {
        Some(b) if b > 0 => Some(b as u64),
        _ => None,
    }
}

pub struct MemoryManager {
    /// NUMA node -> total memory capacity in bytes, read once at startup.
    capacity: BTreeMap<u32, u64>,
    /// `"sandbox_id/container_name" -> (NUMA node, bytes pinned there)`.
    pinned: Mutex<HashMap<String, (u32, u64)>>,
}

impl MemoryManager {
    pub fn new(capacity: BTreeMap<u32, u64>) -> Self {
        Self { capacity, pinned: Mutex::new(HashMap::new()) }
    }

    fn used_per_node(&self, guard: &HashMap<String, (u32, u64)>) -> HashMap<u32, u64> {
        let mut used = HashMap::new();
        for (node, bytes) in guard.values() {
            *used.entry(*node).or_insert(0u64) += bytes;
        }
        used
    }

    /// Currently-unpinned capacity per NUMA node — what
    /// `topology::memory_hint()` needs, and what `allocate_preferring()`
    /// itself picks from.
    pub fn free_per_node(&self) -> BTreeMap<u32, u64> {
        let guard = self.pinned.lock().unwrap();
        let used = self.used_per_node(&guard);
        self.capacity.iter().map(|(node, cap)| (*node, cap.saturating_sub(used.get(node).copied().unwrap_or(0)))).collect()
    }

    /// Try to pin `bytes` of memory for `key` (the same
    /// `"sandbox_id/container_name"` shape every other side table in this
    /// codebase uses) to a single NUMA node — lowest-numbered node with
    /// enough free capacity. `None` if no single node has room (caller
    /// falls back to leaving `cpuset_mems` unset, same graceful-
    /// degradation posture `cpu_manager.rs::allocate()` has).
    pub fn allocate(&self, key: &str, bytes: u64) -> Option<u32> {
        self.allocate_preferring(key, bytes, None)
    }

    /// Same as `allocate()`, but tries `preferred` (Topology Manager's
    /// aligned node — see `topology.rs`) first before falling back to the
    /// lowest-numbered node with enough room.
    pub fn allocate_preferring(&self, key: &str, bytes: u64, preferred: Option<u32>) -> Option<u32> {
        let mut guard = self.pinned.lock().unwrap();
        let used = self.used_per_node(&guard);
        let free = |node: u32| -> u64 { self.capacity.get(&node).copied().unwrap_or(0).saturating_sub(used.get(&node).copied().unwrap_or(0)) };

        let chosen = preferred
            .filter(|&node| free(node) >= bytes)
            .or_else(|| self.capacity.keys().find(|&&node| free(node) >= bytes).copied())?;

        guard.insert(key.to_string(), (chosen, bytes));
        Some(chosen)
    }

    /// Release `key`'s pinned memory, if it has any.
    pub fn release(&self, key: &str) {
        self.pinned.lock().unwrap().remove(key);
    }

    /// Release every pinned claim under `sandbox_id` — mirrors
    /// `CpuManager::release_sandbox()`.
    pub fn release_sandbox(&self, sandbox_id: &str) {
        let prefix = format!("{sandbox_id}/");
        self.pinned.lock().unwrap().retain(|k, _| !k.starts_with(&prefix));
    }

    /// Whether `key` currently holds a pinned-memory claim.
    pub fn is_pinned(&self, key: &str) -> bool {
        self.pinned.lock().unwrap().contains_key(key)
    }

    /// `key`'s currently pinned `(NUMA node, bytes)`, if any — the
    /// PodResources API (round 74) needs the actual assignment, not just
    /// `is_pinned()`'s bool.
    pub fn assigned(&self, key: &str) -> Option<(u32, u64)> {
        self.pinned.lock().unwrap().get(key).copied()
    }

    /// Total capacity per NUMA node, as configured at startup — the
    /// PodResources API's `GetAllocatableResources` needs this alongside
    /// `free_per_node()`'s live view.
    pub fn capacity_per_node(&self) -> BTreeMap<u32, u64> {
        self.capacity.clone()
    }
}

#[cfg(test)]
#[path = "memory_manager_tests/wants_pinned_memory.rs"]
mod tests_wants_pinned_memory;
#[cfg(test)]
#[path = "memory_manager_tests/allocation.rs"]
mod tests_allocation;
