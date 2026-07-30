//! Node-pressure eviction: before this, nodelet only *reported*
//! MemoryPressure/DiskPressure conditions (see `metrics.rs`) — nothing ever
//! acted on them. Real kubelet's eviction manager reclaims resources by
//! terminating pods when a node is under pressure; this is a scoped version
//! of that, not the full thing (see docs/GAP_CLOSURE.md for what's
//! simplified).
//!
//! Deliberately conservative, since this can only rank by what's actually
//! knowable from the Pod spec (no live per-pod cgroup usage stats exist yet
//! — that's the `/stats/summary` gap, tracked separately):
//!   - Only `BestEffort`/`Burstable` pods are ever evicted; `Guaranteed`
//!     pods and anything with a `system-node-critical`/
//!     `system-cluster-critical` priority class are never touched.
//!   - Within the eligible pods, `BestEffort` goes before `Burstable`
//!     (matches real kubelet's QoS-based ranking), and ties are broken by
//!     requested memory (a proxy for "biggest consumer" — the closest
//!     approximation available without real usage stats).
//!   - One pod is evicted per check, not a mass cull — the next tick
//!     re-measures pressure and decides again.

use k8s_openapi::api::core::v1::{Container, Pod, ResourceRequirements};
use std::cmp::Reverse;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum QosClass {
    // Order matters: derives Ord as BestEffort < Burstable < Guaranteed, so
    // BestEffort naturally sorts first as "most evictable".
    BestEffort,
    Burstable,
    Guaranteed,
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
pub fn pick_eviction_candidate(pods: &[Pod]) -> Option<&Pod> {
    pods.iter()
        .filter(|p| {
            p.metadata.deletion_timestamp.is_none() && !is_critical(p) && qos_class(p) != QosClass::Guaranteed
        })
        .min_by_key(|p| (qos_class(p), Reverse(requested_memory_bytes(p))))
}

#[cfg(test)]
#[path = "eviction_tests/qos_class.rs"]
mod tests_qos_class;
#[cfg(test)]
#[path = "eviction_tests/pick_candidate.rs"]
mod tests_pick_candidate;
