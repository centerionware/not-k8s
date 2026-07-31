//! Topology Manager: coordinates CPU Manager (`cpu_manager.rs`, rounds
//! 15-16), device plugins (`device_plugins.rs`, round 14), and Memory
//! Manager (`memory_manager.rs`, round 18) so a container's exclusive
//! CPUs, allocated devices, and pinned memory all land on the *same* NUMA
//! node instead of being picked independently — the whole point of
//! pinning cores is defeated if the pod's GPU (or its own memory) sits on
//! a different NUMA node than its dedicated CPUs, since cross-node memory
//! access is exactly the latency real kubelet's Topology Manager exists
//! to avoid.
//!
//! Opt-in, matching upstream: `NODELET_TOPOLOGY_MANAGER_POLICY` defaults
//! to `none` — this module isn't even consulted in that case.
//!
//! **Scoped as a first slice, not the full hint-generation/bitmask-
//! permutation algorithm real kubelet's Topology Manager runs.** Upstream
//! generates a hint per provider (each hint a NUMA-node bitmask + whether
//! it's "preferred"), then evaluates every cross-provider combination to
//! find the narrowest mutually-preferred alignment. This implementation
//! computes, per provider, the *set of individual NUMA nodes* that alone
//! can satisfy that provider's request, then intersects those sets across
//! providers — a single-node-only algorithm, not the general bitmask one.
//! It reaches the same answer as upstream whenever a single NUMA node can
//! satisfy everything (the common case, and the only case
//! `single-numa-node` policy ever accepts anyway), but won't find a valid
//! *multi*-node alignment upstream's `restricted` policy would accept —
//! this treats `restricted` the same as `single-numa-node` (reject if no
//! single node works) rather than upstream's wider multi-node allowance.
//! Documented here rather than silently thin.
//!
//! As of round 18, Memory Manager is a third hint provider alongside CPU
//! Manager and device plugins — see `memory_hint()` below.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TopologyManagerPolicy {
    /// Coordination is off entirely — matches upstream's own default.
    None,
    /// Prefer an aligned NUMA node when one's achievable; never reject the
    /// pod if it isn't.
    BestEffort,
    /// Reject the pod if no single NUMA node can satisfy every hint
    /// provider's request (see the module doc comment for how this
    /// differs from upstream's wider multi-node `restricted`).
    Restricted,
    /// Same rejection behavior as `Restricted` in this implementation —
    /// upstream distinguishes these two policies via the multi-node
    /// allowance neither policy gets here.
    SingleNumaNode,
}

/// Parse a Linux `cpulist`-format range string (`"0-3"`, `"0-2,5,7-9"` —
/// the same format `/sys/devices/system/node/node*/cpulist` and cgroup
/// v2's own `cpuset.cpus` use) into the CPU IDs it names. The inverse of
/// `cpu_manager::format_cpuset`. Malformed entries are skipped rather than
/// failing the whole parse — a `/sys` read is either fully trustworthy or
/// this returns an empty/partial set, never panics.
pub fn parse_cpulist(s: &str) -> BTreeSet<u32> {
    let mut out = BTreeSet::new();
    for part in s.trim().split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        if let Some((start, end)) = part.split_once('-') {
            if let (Ok(start), Ok(end)) = (start.parse::<u32>(), end.parse::<u32>()) {
                if start <= end {
                    out.extend(start..=end);
                }
            }
        } else if let Ok(id) = part.parse::<u32>() {
            out.insert(id);
        }
    }
    out
}

/// Read the node's NUMA topology from `sys_root` (real callers pass
/// `/sys/devices/system/node`; tests point this at a scratch directory) —
/// NUMA node ID -> the CPU IDs it owns. Returns an empty map (not an
/// error) on any failure — a host without NUMA info (most single-socket
/// edge devices) or without `/sys` access at all should still run
/// everything else normally, just with Topology Manager unable to find
/// any alignment (falls back to "any node," see `align()` below).
pub fn read_numa_topology(sys_root: &Path) -> BTreeMap<u32, BTreeSet<u32>> {
    let Ok(entries) = std::fs::read_dir(sys_root) else { return BTreeMap::new() };
    let mut nodes = BTreeMap::new();
    for entry in entries.filter_map(|e| e.ok()) {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let Some(id_str) = name.strip_prefix("node") else { continue };
        let Ok(id) = id_str.parse::<u32>() else { continue };
        let cpulist_path = entry.path().join("cpulist");
        let Ok(cpulist) = std::fs::read_to_string(&cpulist_path) else { continue };
        nodes.insert(id, parse_cpulist(&cpulist));
    }
    nodes
}

/// Read each NUMA node's total memory capacity from `sys_root` (real
/// callers pass `/sys/devices/system/node`) — NUMA node ID -> bytes,
/// parsed from `node*/meminfo`'s `"Node <id> MemTotal: <N> kB"` first
/// line. Same failure posture as `read_numa_topology()`: an empty map on
/// any read failure, never an error, so Memory Manager just finds no
/// alignment/capacity rather than nodelet refusing to start.
pub fn read_numa_memory(sys_root: &Path) -> BTreeMap<u32, u64> {
    let Ok(entries) = std::fs::read_dir(sys_root) else { return BTreeMap::new() };
    let mut nodes = BTreeMap::new();
    for entry in entries.filter_map(|e| e.ok()) {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let Some(id_str) = name.strip_prefix("node") else { continue };
        let Ok(id) = id_str.parse::<u32>() else { continue };
        let meminfo_path = entry.path().join("meminfo");
        let Ok(meminfo) = std::fs::read_to_string(&meminfo_path) else { continue };
        if let Some(bytes) = parse_node_mem_total(&meminfo) {
            nodes.insert(id, bytes);
        }
    }
    nodes
}

/// Parse the `MemTotal` value (in bytes) out of one NUMA node's `meminfo`
/// file content — pure so it's testable without a real `/sys` tree.
fn parse_node_mem_total(meminfo: &str) -> Option<u64> {
    for line in meminfo.lines() {
        if let Some(rest) = line.split("MemTotal:").nth(1) {
            let kb: u64 = rest.trim().strip_suffix("kB")?.trim().parse().ok()?;
            return Some(kb * 1024);
        }
    }
    None
}

/// Which NUMA nodes have at least `bytes` of memory still free — Memory
/// Manager's hint-provider contribution, pure so it's testable without a
/// live `MemoryManager`. `free_per_node` is the current unpinned capacity
/// per node `MemoryManager::free_per_node()` reports.
pub fn memory_hint(free_per_node: &BTreeMap<u32, u64>, bytes: u64) -> BTreeSet<u32> {
    free_per_node.iter().filter(|(_, free)| **free >= bytes).map(|(node, _)| *node).collect()
}

/// Which NUMA nodes have at least `count` of `available` CPUs still free —
/// CPU Manager's hint-provider contribution, pure so it's testable without
/// a live `CpuManager`. `numa_topology` maps NUMA node -> every CPU it
/// owns; `available` is the current shared-pool-eligible (unreserved,
/// unclaimed) CPU set `CpuManager` would pick from.
pub fn cpu_hint(numa_topology: &BTreeMap<u32, BTreeSet<u32>>, available: &BTreeSet<u32>, count: u32) -> BTreeSet<u32> {
    numa_topology
        .iter()
        .filter(|(_, cpus)| cpus.intersection(available).count() as u32 >= count)
        .map(|(node, _)| *node)
        .collect()
}

/// Which NUMA nodes can supply `count` devices for one resource, given
/// each candidate device's own NUMA affinity (`None` meaning the device
/// plugin didn't report one, which upstream — and this — treats as
/// "compatible with every node," not "compatible with none"). Pure, same
/// reason as `cpu_hint`.
pub fn device_hint(device_numa_nodes: &[Option<u32>], all_numa_nodes: &BTreeSet<u32>, count: u32) -> BTreeSet<u32> {
    if device_numa_nodes.len() < count as usize {
        return BTreeSet::new(); // not enough devices at all, regardless of node
    }
    let mut per_node_counts: BTreeMap<u32, u32> = BTreeMap::new();
    let mut untagged = 0u32;
    for numa in device_numa_nodes {
        match numa {
            Some(node) => *per_node_counts.entry(*node).or_insert(0) += 1,
            None => untagged += 1,
        }
    }
    all_numa_nodes
        .iter()
        .filter(|node| per_node_counts.get(node).copied().unwrap_or(0) + untagged >= count)
        .copied()
        .collect()
}

/// Intersect every hint-provider's candidate-node set and return the
/// lowest-numbered common node, if any — the "which single NUMA node
/// satisfies everyone" answer `create_and_start_container` needs before
/// calling `CpuManager::allocate()`/`DevicePlugins::allocate()`. `hints`
/// with no entries at all (a container with neither an exclusive-CPU nor
/// a device-plugin request) has nothing to align, so this returns `None`
/// — not an error, just "no preference."
pub fn align(hints: &[BTreeSet<u32>]) -> Option<u32> {
    let mut iter = hints.iter();
    let first = iter.next()?.clone();
    let common = iter.fold(first, |acc, h| acc.intersection(h).copied().collect::<BTreeSet<u32>>());
    common.into_iter().next()
}

#[cfg(test)]
#[path = "topology_tests/parse_cpulist.rs"]
mod tests_parse_cpulist;
#[cfg(test)]
#[path = "topology_tests/read_numa_topology.rs"]
mod tests_read_numa_topology;
#[cfg(test)]
#[path = "topology_tests/read_numa_memory.rs"]
mod tests_read_numa_memory;
#[cfg(test)]
#[path = "topology_tests/hints_and_align.rs"]
mod tests_hints_and_align;
