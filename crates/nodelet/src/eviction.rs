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
//!     are broken by whether the pod's actual usage exceeds its own memory
//!     request (round 99 — real kubelet's `exceedMemoryRequests` comparator
//!     step, its actual *primary* ranking criterion upstream, applied here
//!     as a tie-break within the QoS tiers this codebase already has), then
//!     by `spec.priority` (round 26 — lower priority evicted first,
//!     matching real kubelet's own `priority` step in its
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

/// `spec.activeDeadlineSeconds` (round 81; found in round 80's re-audit)
/// — real kubelet's own job (not a controller's), independent of both
/// eviction and `restartPolicy`: once a pod has been running longer than
/// this many seconds since it started, it's terminated regardless of
/// whether it would otherwise keep restarting under `Always`/`OnFailure`.
/// `seconds_since_start` is `None` whenever the pod hasn't recorded a
/// `status.startTime` yet — never treated as "already exceeded" from
/// missing data, same posture `exceeds_ephemeral_storage_limit()` takes.
pub fn active_deadline_exceeded(active_deadline_seconds: Option<i64>, seconds_since_start: Option<i64>) -> bool {
    matches!((active_deadline_seconds, seconds_since_start), (Some(deadline), Some(elapsed)) if elapsed >= deadline)
}

/// Every `emptyDir` volume with `sizeLimit` set, as `(volume name, limit
/// bytes)` (round 67; found in round 65's fresh gap re-audit) — real
/// kubelet enforces this per-volume, distinct from the whole-pod
/// `ephemeral-storage` limit above (a pod can have no ephemeral-storage
/// limit at all and still have one specific `emptyDir` capped). Scoped
/// to plain-disk `emptyDir` only: a `Memory`-medium (tmpfs) or
/// `HugePages`-medium `emptyDir`'s `sizeLimit` is already a real
/// kernel-enforced cap at mount time (`mount -t tmpfs/hugetlbfs -o
/// size=...`, rounds 30/61) — writes past it just fail with `ENOSPC`,
/// there's nothing for periodic measurement-based eviction to add. Pure
/// so this is unit-testable without a live volume directory.
pub fn empty_dir_size_limits(pod: &Pod) -> Vec<(String, u64)> {
    let Some(volumes) = pod.spec.as_ref().and_then(|s| s.volumes.as_ref()) else { return Vec::new() };
    volumes
        .iter()
        .filter_map(|v| {
            let empty_dir = v.empty_dir.as_ref()?;
            let is_disk_backed = empty_dir.medium.as_deref().unwrap_or("").is_empty();
            if !is_disk_backed {
                return None;
            }
            let limit = quantity_value(&empty_dir.size_limit.as_ref()?.0)?;
            Some((v.name.clone(), limit.max(0.0) as u64))
        })
        .collect()
}

/// The name of the first `emptyDir` volume (in `limits`' order) whose
/// measured usage exceeds its own `sizeLimit`, or `None` if every volume
/// with a limit is still within it (or its usage wasn't measured at all
/// — same "never guess a violation from missing data" posture as
/// `exceeds_ephemeral_storage_limit()`). Pure given an already-measured
/// usage map, so unit-testable without a live volume directory.
pub fn first_empty_dir_over_limit(limits: &[(String, u64)], usage_bytes: &std::collections::HashMap<String, u64>) -> Option<String> {
    limits.iter().find(|(name, limit)| usage_bytes.get(name).is_some_and(|usage| usage > limit)).map(|(name, _)| name.clone())
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

/// Real kubelet's `exceedMemoryRequests` comparator step (round 99;
/// found as a documented gap in round 26's own notes) — a pod whose
/// actual memory usage exceeds its own request
/// is ranked as more evictable than one that doesn't, ahead of the
/// `spec.priority` tie-break. Usage unknown (no live stats yet — the
/// mock runtime, or a pod too new for CRI to have measured) is treated
/// as "exceeds", the same conservative direction upstream's own
/// `!found` branch takes (`cmpBool(!p1Found, !p2Found)` prioritizes
/// evicting the pod nodelet has no visibility into over one it can
/// positively confirm is within its request).
fn exceeds_memory_requests(pod: &Pod, usage_bytes_by_uid: &HashMap<String, u64>) -> bool {
    let usage = pod.metadata.uid.as_deref().and_then(|uid| usage_bytes_by_uid.get(uid).copied());
    match usage {
        Some(usage) => usage > requested_memory_bytes(pod),
        None => true,
    }
}

/// Sort key for `pick_eviction_candidate()` — `min_by_key` picks the
/// smallest, so this is ordered "most evictable first": QoS class
/// (`BestEffort` < `Burstable`), then whether usage exceeds the pod's
/// own memory request (round 99 — `Reverse` so "exceeds" sorts first),
/// then `spec.priority` ascending (lower priority is more evictable —
/// round 26), then usage descending via `Reverse` (higher usage is more
/// evictable, the final tie-breaker).
fn eviction_rank(pod: &Pod, usage_bytes_by_uid: &HashMap<String, u64>) -> (QosClass, Reverse<bool>, i32, Reverse<u64>) {
    (
        qos_class(pod),
        Reverse(exceeds_memory_requests(pod, usage_bytes_by_uid)),
        pod_priority(pod),
        Reverse(eviction_weight(pod, usage_bytes_by_uid)),
    )
}

/// Real kubelet's `--eviction-soft`/`--eviction-soft-grace-period` pair
/// (round 101; found in round 99's own notes as the eviction bullet's one
/// remaining explicit simplification, "no soft-threshold grace period").
/// A signal past its *hard* threshold (`hard_true`) evicts this tick,
/// unchanged from pre-round-101 behavior. One only past the looser *soft*
/// threshold must stay continuously true first — `soft_true_since` is how
/// long ago (if ever) it most recently became soft-true without
/// interruption, threaded in/out by the caller (`eviction_loop()`) across
/// ticks: `None` means "not soft-true right now", so nothing to track.
pub fn pressure_action_due(hard_true: bool, soft_true_since: Option<std::time::Duration>, soft_grace_period: std::time::Duration) -> bool {
    hard_true || matches!(soft_true_since, Some(elapsed) if elapsed >= soft_grace_period)
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
#[cfg(test)]
#[path = "eviction_tests/empty_dir_size_limit.rs"]
mod tests_empty_dir_size_limit;
#[cfg(test)]
#[path = "eviction_tests/active_deadline.rs"]
mod tests_active_deadline;
#[cfg(test)]
#[path = "eviction_tests/pressure_action_due.rs"]
mod tests_pressure_action_due;
