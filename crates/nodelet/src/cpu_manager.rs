//! CPU Manager: real kubelet's `--cpu-manager-policy=static` — exclusive,
//! pinned CPU cores for Guaranteed-QoS containers requesting a whole
//! number of CPUs, instead of every container sharing the same CFS-
//! scheduled pool regardless of QoS. Real value for nodelet's edge/
//! latency-sensitive target: a Guaranteed pod asking for `cpu: "2"` can
//! now actually get two cores to itself, not just a CFS quota that still
//! lets the kernel schedule it anywhere alongside everything else.
//!
//! Opt-in, matching upstream: `NODELET_CPU_MANAGER_POLICY` defaults to
//! `none` (kubelet's own default) — `static` turns this on.
//!
//! **Bidirectional, matching real kubelet's static policy** (closed round
//! 16's gap from the initial round-15 slice): when a container claims
//! exclusive cores, every *other already-running* shared-pool container
//! gets its `cpuset.cpus` retroactively shrunk to exclude them — and
//! grown back when the claim is released — via CRI's
//! `UpdateContainerResources` (`runtime/cri.rs::refresh_shared_pool_cpusets()`,
//! called after every `allocate()`/`release()`). This module itself only
//! tracks *which* CPUs are claimed by *which* key; it has no CRI client of
//! its own — the actual `UpdateContainerResources` calls, and the
//! "what were this container's other resource fields" bookkeeping needed
//! to avoid clobbering them, live in `runtime/cri.rs` since only it has
//! the CRI connection and per-container state.
//!
//! Not implemented: Topology Manager (NUMA-aware coordination between
//! this, device plugins' `TopologyInfo`, and memory placement) and Memory
//! Manager. CPU selection here is simple ascending-CPU-ID, not topology-
//! aware core/socket packing.

use crate::eviction::QosClass;
use std::collections::{BTreeSet, HashMap};
use std::sync::Mutex;

/// Whether a container's QoS class + CPU limit make it eligible for
/// exclusive CPU pinning — real kubelet's static policy rule: Guaranteed
/// QoS (the pod-level check happens once, by the caller) and a CPU limit
/// that's a whole, positive number of cores. Returns the core count to
/// allocate, or `None` if the container should just use the shared pool
/// (BestEffort/Burstable, or a fractional/zero Guaranteed CPU request).
pub fn wants_exclusive_cpus(qos: QosClass, cpu_millicores: Option<i64>) -> Option<u32> {
    if qos != QosClass::Guaranteed {
        return None;
    }
    match cpu_millicores {
        Some(m) if m > 0 && m % 1000 == 0 => Some((m / 1000) as u32),
        _ => None,
    }
}

/// How many of the lowest-numbered CPUs are reserved for the host/kubelet
/// itself (never eligible for the shared pool or exclusive assignment) —
/// derived from `system-reserved` + `kube-reserved` CPU millicores,
/// rounded up to a whole core, matching real kubelet's own fallback
/// (`--reserved-cpus` unset) behavior of reserving whole cores for a
/// fractional reservation.
pub fn reserved_cpu_count(reserved_millicores: u64) -> u32 {
    reserved_millicores.div_ceil(1000) as u32
}

/// Render a CPU ID set as CRI's `cpuset_cpus` syntax: ascending ranges
/// joined by commas (`"0-2,5,7-9"`), matching the Linux `cpuset.cpus`
/// cgroup file format directly — CRI passes this string straight through
/// to the runtime, no further translation.
pub fn format_cpuset(cpus: &BTreeSet<u32>) -> String {
    let mut parts = Vec::new();
    let mut iter = cpus.iter().copied().peekable();
    while let Some(start) = iter.next() {
        let mut end = start;
        while iter.peek() == Some(&(end + 1)) {
            end = iter.next().unwrap();
        }
        if start == end {
            parts.push(start.to_string());
        } else {
            parts.push(format!("{start}-{end}"));
        }
    }
    parts.join(",")
}

pub struct CpuManager {
    total_cpus: BTreeSet<u32>,
    reserved: BTreeSet<u32>,
    /// `"sandbox_id/container_name" -> exclusively assigned CPU IDs`.
    exclusive: Mutex<HashMap<String, BTreeSet<u32>>>,
}

impl CpuManager {
    /// `total_cores` is the node's whole-core count (`Config::cpu_cores`);
    /// `reserved_millicores` is `system_reserved + kube_reserved` CPU.
    pub fn new(total_cores: u64, reserved_millicores: u64) -> Self {
        let total_cpus: BTreeSet<u32> = (0..total_cores as u32).collect();
        let reserved_count = reserved_cpu_count(reserved_millicores).min(total_cores as u32);
        let reserved: BTreeSet<u32> = (0..reserved_count).collect();
        Self { total_cpus, reserved, exclusive: Mutex::new(HashMap::new()) }
    }

    fn all_exclusive(&self, guard: &HashMap<String, BTreeSet<u32>>) -> BTreeSet<u32> {
        guard.values().flatten().copied().collect()
    }

    /// The pool every non-exclusive container's `cpuset_cpus` should be
    /// set to right now: everything except reserved and currently
    /// exclusively-claimed CPUs.
    pub fn shared_pool(&self) -> BTreeSet<u32> {
        let guard = self.exclusive.lock().unwrap();
        let claimed = self.all_exclusive(&guard);
        self.total_cpus.difference(&self.reserved).filter(|c| !claimed.contains(c)).copied().collect()
    }

    /// Every CPU eligible for exclusive allocation at all (total minus
    /// reserved), regardless of current claims — what the PodResources
    /// API's `GetAllocatableResources` reports (round 74; matches real
    /// kubelet's own semantics: the whole static-policy-managed pool, not
    /// just what's free right now).
    pub fn allocatable_cpus(&self) -> BTreeSet<u32> {
        self.total_cpus.difference(&self.reserved).copied().collect()
    }

    /// Try to claim `count` exclusive CPUs for `key` (the same
    /// `"sandbox_id/container_name"` shape `restart_counts`/
    /// `device_allocations` use) — lowest-numbered-first from the current
    /// shared pool, not topology-aware. `None` if the shared pool can't
    /// satisfy it (caller falls back to the shared pool for this
    /// container instead of failing it outright — nodelet has no
    /// pre-admission step to reject the pod before it reaches the node,
    /// so graceful degradation beats a hard failure here).
    pub fn allocate(&self, key: &str, count: u32) -> Option<BTreeSet<u32>> {
        self.allocate_preferring(key, count, None)
    }

    /// Same as `allocate()`, but tries `preferred` CPUs first (the aligned
    /// NUMA node's own CPUs, computed by `topology.rs` — see
    /// `runtime/cri.rs`'s Topology Manager wiring) before falling back to
    /// the rest of the shared pool if `preferred` alone can't satisfy
    /// `count`. `preferred = None` is exactly `allocate()`'s plain
    /// lowest-numbered-first behavior (Topology Manager disabled, or no
    /// alignment was found).
    pub fn allocate_preferring(&self, key: &str, count: u32, preferred: Option<&BTreeSet<u32>>) -> Option<BTreeSet<u32>> {
        let mut guard = self.exclusive.lock().unwrap();
        let claimed = self.all_exclusive(&guard);
        let free: BTreeSet<u32> = self.total_cpus.difference(&self.reserved).filter(|c| !claimed.contains(c)).copied().collect();

        let mut picked: BTreeSet<u32> = match preferred {
            Some(preferred) => free.intersection(preferred).take(count as usize).copied().collect(),
            None => BTreeSet::new(),
        };
        for cpu in &free {
            if picked.len() >= count as usize {
                break;
            }
            picked.insert(*cpu);
        }
        if picked.len() < count as usize {
            return None;
        }
        guard.insert(key.to_string(), picked.clone());
        Some(picked)
    }

    /// Release `key`'s exclusive claim, if it has one — back into the
    /// shared pool. The caller (`runtime/cri.rs`) is responsible for then
    /// calling `refresh_shared_pool_cpusets()` so already-running
    /// shared-pool containers actually get grown back onto these cores.
    pub fn release(&self, key: &str) {
        self.exclusive.lock().unwrap().remove(key);
    }

    /// Release every exclusive claim under `sandbox_id` — mirrors
    /// `release_sandbox_devices()`'s prefix sweep for sandbox teardown/GC.
    pub fn release_sandbox(&self, sandbox_id: &str) {
        let prefix = format!("{sandbox_id}/");
        self.exclusive.lock().unwrap().retain(|k, _| !k.starts_with(&prefix));
    }

    /// Whether `key` currently holds an exclusive claim — `runtime/cri.rs`'s
    /// `refresh_shared_pool_cpusets()` uses this to skip exclusively-pinned
    /// containers when sweeping the shared pool across everything else.
    pub fn is_exclusive(&self, key: &str) -> bool {
        self.exclusive.lock().unwrap().contains_key(key)
    }

    /// `key`'s currently exclusively-assigned CPU IDs, if any — the
    /// PodResources API (round 74) needs the actual set, not just
    /// `is_exclusive()`'s bool.
    pub fn assigned(&self, key: &str) -> Option<BTreeSet<u32>> {
        self.exclusive.lock().unwrap().get(key).cloned()
    }
}

/// Parse `NODELET_CPU_MANAGER_POLICY` — `"none"` (default, matches
/// upstream) or `"static"`. Any other value is treated as `none` with a
/// warning at the config layer, not here (this just models the two valid
/// states).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CpuManagerPolicy {
    None,
    Static,
}

#[cfg(test)]
#[path = "cpu_manager_tests/wants_exclusive_cpus.rs"]
mod tests_wants_exclusive_cpus;
#[cfg(test)]
#[path = "cpu_manager_tests/format_cpuset.rs"]
mod tests_format_cpuset;
#[cfg(test)]
#[path = "cpu_manager_tests/allocation.rs"]
mod tests_allocation;
