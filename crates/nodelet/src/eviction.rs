//! Node-pressure eviction: before this, nodelet only *reported*
//! MemoryPressure/DiskPressure conditions (see `metrics.rs`) — nothing ever
//! acted on them. Real kubelet's eviction manager reclaims resources by
//! terminating pods when a node is under pressure; this is a scoped version
//! of that, not the full thing (see docs/GAP_CLOSURE.md for what's
//! simplified).
//!
//! Deliberately conservative:
//!   - Only `BestEffort`/`Burstable` pods are ever evicted; `Guaranteed`
//!     pods and anything with a `system-node-critical`/
//!     `system-cluster-critical` priority class are never touched.
//!   - Within the eligible pods, `BestEffort` goes before `Burstable`
//!     (matches real kubelet's QoS-based ranking); ties within a QoS class
//!     are broken by `spec.priority` (round 26 — lower priority evicted
//!     first, matching real kubelet's own `priority` step in its
//!     `rankMemoryPressure`/`rankDiskPressureFunc` comparator chains), and
//!     ties within the *same* priority are broken by real memory usage
//!     (from CRI's `ListPodSandboxStats`, the same source
//!     `server::stats`'s `/stats/summary` uses) when known, falling back to
//!     *requested* memory for any pod without live stats yet (e.g. the mock
//!     runtime, or a pod too new for CRI to have measured). `spec.priority`
//!     is read directly off the Pod object — the apiserver's own Priority
//!     admission controller already resolves `priorityClassName` to a
//!     numeric value there before nodelet ever sees the pod, so no
//!     `PriorityClass` lookup is needed.
//!   - One pod is evicted per check, not a mass cull — the next tick
//!     re-measures pressure and decides again.

use k8s_openapi::api::core::v1::{Container, Pod, ResourceRequirements};
use std::cmp::Reverse;
use std::collections::HashMap;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum QosClass {
    // Order matters: derives Ord as BestEffort < Burstable < Guaranteed, so
    // BestEffort naturally sorts first as "most evictable".
    BestEffort,
    Burstable,
    Guaranteed,
}

impl QosClass {
    /// The exact string real kubelet reports on `PodStatus.qosClass`
    /// (round 55; found in round 54's re-audit) — matches the Kubernetes
    /// API's own `PodQOSClass` constants verbatim.
    pub fn as_str(self) -> &'static str {
        match self {
            QosClass::BestEffort => "BestEffort",
            QosClass::Burstable => "Burstable",
            QosClass::Guaranteed => "Guaranteed",
        }
    }
}

/// Parse a `cpu`/`memory` Quantity string to a comparable numeric value.
/// Good enough for QoS-class equality/presence checks — not a full
/// arbitrary-precision Quantity parser.
fn quantity_value(s: &str) -> Option<f64> {
    let s = s.trim();
    if let Some(m) = s.strip_suffix('m') {
        return m.parse::<f64>().ok().map(|v| v / 1000.0);
    }
    const BINARY: [(&str, f64); 4] =
        [("Ki", 1024.0), ("Mi", 1024.0 * 1024.0), ("Gi", 1024.0 * 1024.0 * 1024.0), ("Ti", 1024.0 * 1024.0 * 1024.0 * 1024.0)];
    const DECIMAL: [(&str, f64); 4] = [("k", 1e3), ("M", 1e6), ("G", 1e9), ("T", 1e12)];
    for (suf, mult) in BINARY.into_iter().chain(DECIMAL) {
        if let Some(num) = s.strip_suffix(suf) {
            return num.parse::<f64>().ok().map(|n| n * mult);
        }
    }
    s.parse::<f64>().ok()
}

fn resource_value(resources: Option<&ResourceRequirements>, which: &str, resource: &str) -> Option<f64> {
    let map = match which {
        "requests" => resources?.requests.as_ref()?,
        _ => resources?.limits.as_ref()?,
    };
    map.get(resource).and_then(|q| quantity_value(&q.0))
}

/// Real kubelet's QoS algorithm, restricted to cpu/memory (extended
/// resources also count upstream; skipped here — this only needs to rank
/// eviction order, not replicate the admission-time QoS field exactly).
pub fn qos_class(pod: &Pod) -> QosClass {
    let containers: Vec<&Container> = pod
        .spec
        .as_ref()
        .map(|s| s.containers.iter().chain(s.init_containers.iter().flatten()).collect())
        .unwrap_or_default();
    if containers.is_empty() {
        return QosClass::BestEffort;
    }

    let mut any_request_or_limit = false;
    let mut all_guaranteed = true;

    for c in &containers {
        for resource in ["cpu", "memory"] {
            let request = resource_value(c.resources.as_ref(), "requests", resource);
            let limit = resource_value(c.resources.as_ref(), "limits", resource);
            if request.is_some() || limit.is_some() {
                any_request_or_limit = true;
            }
            // Guaranteed requires both set and equal for every resource on
            // every container; a limit with no matching (or defaulted)
            // request, or a request without a limit, disqualifies it.
            match (request.or(limit), limit) {
                (Some(r), Some(l)) if r == l => {}
                _ => all_guaranteed = false,
            }
        }
    }

    if !any_request_or_limit {
        QosClass::BestEffort
    } else if all_guaranteed {
        QosClass::Guaranteed
    } else {
        QosClass::Burstable
    }
}

/// Total requested memory across a pod's containers (falling back to
/// limits where there's no request) — the tie-breaker within a QoS class
/// when there's no real usage number to rank by.
fn requested_memory_bytes(pod: &Pod) -> u64 {
    pod.spec
        .as_ref()
        .map(|s| s.containers.iter().chain(s.init_containers.iter().flatten()).collect::<Vec<_>>())
        .unwrap_or_default()
        .iter()
        .filter_map(|c| {
            resource_value(c.resources.as_ref(), "requests", "memory")
                .or_else(|| resource_value(c.resources.as_ref(), "limits", "memory"))
        })
        .sum::<f64>()
        .max(0.0) as u64
}

/// Real kubelet's per-container OOM score adjustment
/// (`pkg/kubelet/qos/policy.go`'s `GetContainerOOMScoreAdjust`, round 28)
/// — how strongly the kernel OOM killer should prefer killing this
/// container under real memory pressure, independent of and faster than
/// `eviction_loop()`'s own check interval. `Guaranteed` and `BestEffort`
/// get real kubelet's fixed values; `Burstable` is scaled by how much of
/// the node's total memory this container's own request claims (a bigger
/// share is less likely to be picked by the kernel first), clamped to
/// `[2, 999]` so it never overlaps `Guaranteed`'s protected negative
/// range or reaches `BestEffort`'s certain-death `1000`.
/// `node_memory_capacity_bytes <= 0` (degenerate, shouldn't happen with a
/// real `/proc/meminfo` read) falls back to `999` — the most-evictable
/// value still inside Burstable's own range, not a panic.
pub fn oom_score_adj(qos: QosClass, container_memory_request_bytes: i64, node_memory_capacity_bytes: i64) -> i64 {
    match qos {
        QosClass::Guaranteed => -998,
        QosClass::BestEffort => 1000,
        QosClass::Burstable => {
            if node_memory_capacity_bytes <= 0 {
                return 999;
            }
            let scaled = 1000 - (1000 * container_memory_request_bytes) / node_memory_capacity_bytes;
            scaled.clamp(2, 999)
        }
    }
}

/// Sum of every container's `resources.limits["ephemeral-storage"]`
/// (round 49; the deferred half of round 48's arc) — real kubelet's own
/// per-pod ephemeral-storage limit is likewise the sum across containers,
/// not a single pod-level field. `None` when no container sets one at
/// all, distinct from `Some(0)` (an explicit `"0"` limit, which a pod
/// with any real usage would immediately violate) — the caller needs to
/// tell "no limit configured" apart from "a zero limit."
pub fn ephemeral_storage_limit_bytes(pod: &Pod) -> Option<u64> {
    let containers: Vec<&Container> = pod
        .spec
        .as_ref()
        .map(|s| s.containers.iter().chain(s.init_containers.iter().flatten()).collect())
        .unwrap_or_default();
    let mut total = 0u64;
    let mut any_set = false;
    for c in &containers {
        if let Some(v) = resource_value(c.resources.as_ref(), "limits", "ephemeral-storage") {
            total += v.max(0.0) as u64;
            any_set = true;
        }
    }
    any_set.then_some(total)
}

/// Whether a pod's measured ephemeral-storage usage exceeds its own
/// configured limit — real kubelet's per-pod eviction trigger, distinct
/// from (and checked ahead of) general node-pressure-based eviction: this
/// fires even when the node overall isn't under `DiskPressure` at all,
/// the same way an individual container gets OOM-killed for exceeding its
/// own memory limit regardless of the node's overall memory state.
/// `false` whenever either input is unknown (no limit configured, or
/// usage couldn't be measured) — never guesses a violation from missing
/// data.
pub fn exceeds_ephemeral_storage_limit(usage_bytes: Option<u64>, limit_bytes: Option<u64>) -> bool {
    matches!((usage_bytes, limit_bytes), (Some(usage), Some(limit)) if usage > limit)
}

/// `spec.priority` — already a resolved numeric value by the time nodelet
/// sees the Pod (the apiserver's Priority admission controller resolves
/// `priorityClassName` into this field at admission time), so no
/// `PriorityClass` object lookup is needed. Defaults to `0`, matching
/// upstream's own default for a pod with no priority class at all.
fn pod_priority(pod: &Pod) -> i32 {
    pod.spec.as_ref().and_then(|s| s.priority).unwrap_or(0)
}

/// `system-node-critical`/`system-cluster-critical` pods are never evicted —
/// matches real kubelet's protection for cluster-essential workloads (e.g.
/// its own kube-system add-ons), even though nodelet doesn't resolve
/// PriorityClass objects to get an actual numeric priority.
pub fn is_critical(pod: &Pod) -> bool {
    matches!(
        pod.spec.as_ref().and_then(|s| s.priority_class_name.as_deref()),
        Some("system-node-critical") | Some("system-cluster-critical")
    )
}

/// Pick the single best eviction candidate among `pods`, or `None` if
/// there's nothing eligible (only Guaranteed/critical/already-terminating
/// pods present — real kubelet would still evict Guaranteed under severe
/// enough pressure; this stays conservative given the ranking is
/// request-based, not usage-based).
/// Rank by real memory usage when it's known (keyed by pod UID — see
/// `main.rs::eviction_loop`, which populates this from
/// `PodRuntime::pod_usage_stats()`), falling back to *requested* memory for
/// any pod not in the map (the mock runtime never populates it at all, and
/// even on `cri` a pod's stats can lag its apiserver object briefly). Real
/// usage is the more accurate signal — a pod that requested little but is
/// actually the node's biggest consumer should be evictable ahead of one
/// that merely asked for more.
fn eviction_weight(pod: &Pod, usage_bytes_by_uid: &HashMap<String, u64>) -> u64 {
    pod.metadata
        .uid
        .as_deref()
        .and_then(|uid| usage_bytes_by_uid.get(uid).copied())
        .unwrap_or_else(|| requested_memory_bytes(pod))
}

/// Sort key for `pick_eviction_candidate()` — `min_by_key` picks the
/// smallest, so this is ordered "most evictable first": QoS class
/// (`BestEffort` < `Burstable`), then `spec.priority` ascending (lower
/// priority is more evictable — round 26), then usage descending via
/// `Reverse` (higher usage is more evictable, the tie-breaker within the
/// same QoS class *and* the same priority).
fn eviction_rank(pod: &Pod, usage_bytes_by_uid: &HashMap<String, u64>) -> (QosClass, i32, Reverse<u64>) {
    (qos_class(pod), pod_priority(pod), Reverse(eviction_weight(pod, usage_bytes_by_uid)))
}

pub fn pick_eviction_candidate<'a>(pods: &'a [Pod], usage_bytes_by_uid: &HashMap<String, u64>) -> Option<&'a Pod> {
    pods.iter()
        .filter(|p| {
            p.metadata.deletion_timestamp.is_none() && !is_critical(p) && qos_class(p) != QosClass::Guaranteed
        })
        .min_by_key(|p| eviction_rank(p, usage_bytes_by_uid))
}

#[cfg(test)]
#[path = "eviction_tests/qos_class.rs"]
mod tests_qos_class;
#[cfg(test)]
#[path = "eviction_tests/pick_candidate.rs"]
mod tests_pick_candidate;
#[cfg(test)]
#[path = "eviction_tests/oom_score_adj.rs"]
mod tests_oom_score_adj;
#[cfg(test)]
#[path = "eviction_tests/ephemeral_storage.rs"]
mod tests_ephemeral_storage;
