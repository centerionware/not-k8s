//! Node cgroup hierarchy: a QoS-scoped `cgroup_parent` per pod sandbox (so
//! pods actually land under `kubepods/<qos>/pod<uid>`, matching real
//! kubelet, instead of an unscoped flat cgroup tree with no relationship
//! between QoS class and cgroup placement) and node allocatable enforcement
//! (capping the top-level `kubepods` cgroup at `Node.status.allocatable`,
//! cgroup v2 only).
//!
//! CRI's own `LinuxPodSandboxConfig.cgroup_parent` proto comment says: "The
//! cgroupfs style syntax will be used, but the container runtime can
//! convert it to systemd semantics if needed" — so this always builds a
//! cgroupfs-style path regardless of what cgroup driver the runtime is
//! actually configured with. That's CRI's documented contract, not a
//! nodelet simplification: it means nodelet doesn't need to detect or
//! configure a cgroup driver at all, unlike real kubelet's
//! `--cgroup-driver` flag.

use crate::eviction::QosClass;
use std::path::Path;
use tracing::warn;

/// Real kubelet's well-known top-level cgroup name for everything it
/// manages — not configurable there, and not made configurable here either
/// (a custom root would only matter for a --cgroup-root real kubelet
/// feature nodelet doesn't otherwise implement).
pub const CGROUP_ROOT_NAME: &str = "kubepods";

/// The cgroupfs-style parent path CRI expects for a pod sandbox, scoped by
/// QoS class exactly like real kubelet: `Guaranteed` pods sit directly
/// under `kubepods` (no QoS subdirectory — a Guaranteed pod's resources are
/// exact, so there's nothing to additionally bound at the QoS level),
/// `Burstable`/`BestEffort` get their own subdirectory so cgroup-aware
/// tooling (and a human debugging with `systemd-cgls`/`cat
/// /sys/fs/cgroup/.../cgroup.procs`) can see QoS grouping at a glance.
pub fn cgroup_parent_for(qos: QosClass, pod_uid: &str) -> String {
    match qos {
        QosClass::Guaranteed => format!("/{CGROUP_ROOT_NAME}/pod{pod_uid}"),
        QosClass::Burstable => format!("/{CGROUP_ROOT_NAME}/burstable/pod{pod_uid}"),
        QosClass::BestEffort => format!("/{CGROUP_ROOT_NAME}/besteffort/pod{pod_uid}"),
    }
}

const CPU_PERIOD_US: u64 = 100_000;

/// cgroup v2 `cpu.max` file content: `"<quota> <period>"`, or the literal
/// `"max"` (no limit) when `millicores` is `0` — same "unset/unlimited"
/// convention `runtime/cri.rs::linux_resources` already uses for
/// per-container CPU limits, applied here to the node-level cgroup.
pub fn cpu_max_line(millicores: u64) -> String {
    if millicores == 0 {
        return "max".to_string();
    }
    let quota = CPU_PERIOD_US * millicores / 1000;
    format!("{quota} {CPU_PERIOD_US}")
}

/// cgroup v2 `memory.max` file content.
pub fn memory_max_line(bytes: u64) -> String {
    if bytes == 0 {
        "max".to_string()
    } else {
        bytes.to_string()
    }
}

/// Create (if missing) and cap the top-level `kubepods` cgroup at the
/// node's allocatable resources — the actual "enforcement" behind real
/// kubelet's `--enforce-node-allocatable=pods` (its own default): once this
/// is set, the *sum* of every pod's cgroup under `kubepods` can never
/// exceed it, regardless of what any individual pod's own limits say.
///
/// Best-effort and non-fatal by design: this needs root and a cgroup v2
/// unified hierarchy with `cpu`/`memory` delegated from `cgroup_fs_root`'s
/// top level. A container without host cgroup access, a cgroup v1 host, or
/// a non-root nodelet process should all still start up and run pods
/// normally — just without this one additional guarantee, logged clearly
/// so it's visible rather than silently absent.
pub fn enforce_node_allocatable(cgroup_fs_root: &str, allocatable_cpu_millicores: u64, allocatable_memory_bytes: u64) {
    let root = Path::new(cgroup_fs_root);
    let kubepods = root.join(CGROUP_ROOT_NAME);

    if let Err(e) = std::fs::create_dir_all(&kubepods) {
        warn!(
            path = %kubepods.display(),
            error = ?e,
            "node allocatable enforcement: couldn't create the kubepods cgroup (needs root + cgroup v2) — pods won't be capped at the node's allocatable resources"
        );
        return;
    }
    // Delegate cpu/memory from the parent so kubepods (and its own
    // children, once cgroup_parent_for()'s paths get populated by the
    // runtime) can actually set cpu.max/memory.max. A no-op, not an error,
    // if already delegated — ignored either way since this is inherently
    // best-effort.
    let _ = std::fs::write(root.join("cgroup.subtree_control"), "+cpu +memory");

    if let Err(e) = std::fs::write(kubepods.join("cpu.max"), cpu_max_line(allocatable_cpu_millicores)) {
        warn!(error = ?e, "node allocatable enforcement: failed to write the kubepods cgroup's cpu.max");
    }
    if let Err(e) = std::fs::write(kubepods.join("memory.max"), memory_max_line(allocatable_memory_bytes)) {
        warn!(error = ?e, "node allocatable enforcement: failed to write the kubepods cgroup's memory.max");
    }
}

#[cfg(test)]
#[path = "cgroup_tests/cgroup_parent_for.rs"]
mod tests_cgroup_parent_for;
#[cfg(test)]
#[path = "cgroup_tests/cpu_max_line.rs"]
mod tests_cpu_max_line;
#[cfg(test)]
#[path = "cgroup_tests/enforce_node_allocatable.rs"]
mod tests_enforce_node_allocatable;
