//! `ResourceQuota` — a faithful-but-substantially-scoped port of real
//! upstream's own admission plugin
//! (`staging/src/k8s.io/apiserver/pkg/admission/plugin/resourcequota/controller.go`
//! + `pkg/quota/v1/evaluator/core/pods.go`, release-1.34, fetched and read
//! directly): forbids a `Pod` `CREATE` that would push its namespace's
//! tracked resource usage over any `ResourceQuota` object's own
//! `spec.hard` limit.
//!
//! **Pods only** — real upstream's `ResourceQuota` also tracks
//! `Service`/`PersistentVolumeClaim`/`Secret`/`ConfigMap`/arbitrary
//! `count/<resource>` object counts through a whole per-type `Evaluator`
//! registry (`pkg/quota/v1/evaluator/core/*.go`); this crate only ports
//! the pod evaluator, real upstream's own `podEvaluator`, which is what
//! the overwhelming majority of real `ResourceQuota` usage actually
//! targets (compute resource limits). **Compute resources only** — of
//! real upstream's own `podResources` tracked-resource list, this port
//! covers `pods` (object count), `cpu`/`requests.cpu`, `memory`/
//! `requests.memory`, `limits.cpu`, `limits.memory`; not ported:
//! `ephemeral-storage` and its `requests`/`limits` forms, the
//! `hugepages-*` family, and extended resources — all real, all just not
//! this crate's first cut.
//!
//! **No scope matching** — real upstream's own `ResourceQuota.spec.scopes`/
//! `scopeSelector` (`Terminating`/`NotTerminating`/`BestEffort`/
//! `NotBestEffort`/`PriorityClass`/`CrossNamespacePodAffinity`) let an
//! operator target a quota at a subset of pods; every `ResourceQuota` in
//! the namespace is treated as unscoped (applies to every pod) here — a
//! real, named under-enforcement (a scope-restricted quota gets applied
//! more broadly than intended, never less, so this errs toward stricter
//! than requested, not laxer).
//!
//! **No persisted `status.used` counter, named honestly**: real
//! upstream's own controller maintains a running `status.used` total on
//! each `ResourceQuota`, updated with real optimistic-concurrency retry
//! as part of admission (`quota.Add(resourceQuota.Status.Used,
//! requestedUsage)`, `quota.LessThanOrEqual` against `Status.Hard`) —
//! this port instead recomputes total usage from scratch on every check
//! (a live `server::rest::list` of every `Pod` in the namespace, summing
//! each non-terminal one's own usage, plus this request's own delta),
//! compared directly against `spec.hard` (there is no `status.hard`
//! sync controller here either, so `spec.hard` is read directly — a real
//! cluster's own `status.hard` converges to exactly that anyway). This is
//! the same "no cache, always live" posture every other plugin in this
//! module already takes, but it carries one real, narrower consequence a
//! persisted counter wouldn't: **a race between two concurrent `CREATE`s
//! that each individually fit under the quota can both be admitted**,
//! together exceeding it — real upstream's own per-quota optimistic-lock
//! retry loop serializes exactly this case; this port doesn't. Named
//! honestly as a real, accepted limitation, not silently glossed over.
//!
//! Real upstream's own denial message format is ported exactly:
//! `"exceeded quota: <name>, requested: <resource>=<val>, used:
//! <resource>=<val>, limited: <resource>=<val>"`, restricted to only the
//! resource(s) that actually exceeded.
//!
//! Reuses [`crate::admission::limit_ranger::pod_requests`]/
//! [`crate::admission::limit_ranger::pod_limits`] for the actual
//! requests/limits aggregation — real upstream's own `PodUsageFunc` calls
//! the exact same `resourcehelper.PodRequests`/`PodLimits` helper
//! `limit_ranger`'s own port already ported once.
//!
//! Same split as every other Group J plugin: pure decision functions
//! (unit tested with no I/O) plus the real I/O steps (`server::rest::list`
//! over `ResourceQuota` and over every `Pod` in the namespace)
//! `server::listener` performs in between.

use crate::admission::limit_ranger::{pod_limits, pod_requests};
use crate::scheme::quantity::Quantity;
use serde_json::{json, Value};
use std::collections::BTreeMap;

pub fn applies_to(operation: crate::admission::attributes::Operation, group: &str, resource: &str, subresource: &str) -> bool {
    group.is_empty() && resource == "pods" && subresource.is_empty() && operation == crate::admission::attributes::Operation::Create
}

/// Real upstream's own `QuotaV1Pod`, minus the `DeletionGracePeriodSeconds`
/// grace-period-expiry half — this crate's admission path never sees a
/// pod mid-deletion (a `CREATE`/existing-pod-list read, never a delete in
/// progress with a live clock to compare against), so only the terminal-
/// phase check applies in practice; kept as its own named subset rather
/// than silently assumed equivalent.
fn counts_toward_quota(pod: &Value) -> bool {
    let phase = pod.get("status").and_then(|s| s.get("phase")).and_then(Value::as_str).unwrap_or("");
    phase != "Failed" && phase != "Succeeded"
}

/// Real upstream's own `podComputeUsageHelper`, restricted to the
/// compute-resource subset this port covers (see this module's own doc
/// comment). `requests`/`limits` are [`pod_requests`]/[`pod_limits`]'s
/// own aggregated output for one pod.
fn pod_compute_usage(requests: &BTreeMap<String, Quantity>, limits: &BTreeMap<String, Quantity>) -> BTreeMap<String, Quantity> {
    let mut usage = BTreeMap::new();
    usage.insert("pods".to_string(), Quantity::parse("1").expect("literal \"1\" always parses"));
    if let Some(&cpu) = requests.get("cpu") {
        usage.insert("cpu".to_string(), cpu);
        usage.insert("requests.cpu".to_string(), cpu);
    }
    if let Some(&cpu) = limits.get("cpu") {
        usage.insert("limits.cpu".to_string(), cpu);
    }
    if let Some(&mem) = requests.get("memory") {
        usage.insert("memory".to_string(), mem);
        usage.insert("requests.memory".to_string(), mem);
    }
    if let Some(&mem) = limits.get("memory") {
        usage.insert("limits.memory".to_string(), mem);
    }
    usage
}

/// Real upstream's own `PodUsageFunc`: the object-count entry (`pods`)
/// is always charged, even for a pod that wouldn't otherwise count
/// (real upstream's own comment: "always quota the object count... even
/// if the pod is end of life") — but this port's caller only ever calls
/// this for pods that already passed [`counts_toward_quota`], since a
/// terminal pod contributes nothing else either way here (no separate
/// `count/pods` resource name tracked — see this module's own doc
/// comment for what's out of scope).
pub fn pod_usage(pod: &Value) -> BTreeMap<String, Quantity> {
    pod_compute_usage(&pod_requests(pod), &pod_limits(pod))
}

fn add_maps(a: &BTreeMap<String, Quantity>, b: &BTreeMap<String, Quantity>) -> BTreeMap<String, Quantity> {
    let mut out = a.clone();
    for (k, v) in b {
        out.entry(k.clone()).and_modify(|e| *e = *e + *v).or_insert(*v);
    }
    out
}

fn hard_limits(resource_quota: &Value) -> BTreeMap<String, Quantity> {
    resource_quota
        .get("spec")
        .and_then(|s| s.get("hard"))
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
        .filter_map(|(name, raw)| Some((name.clone(), Quantity::parse(raw.as_str()?).ok()?)))
        .collect()
}

/// Real upstream's own `MatchingResources`/`Intersection`: whether
/// `resource_quota` tracks at least one resource this pod usage
/// computation could ever produce (`pod_compute_usage`'s own key set) —
/// a `ResourceQuota` whose `spec.hard` only names resources this port
/// doesn't track (e.g. `count/services`) is correctly not consulted at
/// all, rather than spuriously matched.
const TRACKED_RESOURCES: [&str; 7] = ["pods", "cpu", "requests.cpu", "limits.cpu", "memory", "requests.memory", "limits.memory"];

fn quota_applies(resource_quota: &Value) -> bool {
    hard_limits(resource_quota).keys().any(|k| TRACKED_RESOURCES.contains(&k.as_str()))
}

fn format_usage(map: &BTreeMap<String, Quantity>) -> String {
    map.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join(",")
}

/// Real upstream's own `checkRequest` core comparison, ported: for one
/// `ResourceQuota`, would `new_total` (this pod's own usage, added to
/// every other non-terminal pod's usage already in the namespace) exceed
/// `spec.hard` for any resource the quota actually tracks? `None` if not
/// — real upstream's own message format
/// (`"exceeded quota: <name>, requested: ..., used: ..., limited:
/// ..."`), restricted to only the resource(s) that actually exceeded.
fn check_quota(resource_quota: &Value, existing_usage: &BTreeMap<String, Quantity>, this_pod_usage: &BTreeMap<String, Quantity>) -> Option<String> {
    let hard = hard_limits(resource_quota);
    let new_total = add_maps(existing_usage, this_pod_usage);

    let mut exceeded_requested = BTreeMap::new();
    let mut exceeded_used = BTreeMap::new();
    let mut exceeded_hard = BTreeMap::new();
    for (name, &limit) in &hard {
        let Some(&requested) = this_pod_usage.get(name) else { continue };
        let total = new_total.get(name).copied().unwrap_or(Quantity::ZERO);
        if total > limit {
            exceeded_requested.insert(name.clone(), requested);
            exceeded_used.insert(name.clone(), existing_usage.get(name).copied().unwrap_or(Quantity::ZERO));
            exceeded_hard.insert(name.clone(), limit);
        }
    }
    if exceeded_requested.is_empty() {
        return None;
    }
    let name = resource_quota.get("metadata").and_then(|m| m.get("name")).and_then(Value::as_str).unwrap_or("");
    Some(format!("exceeded quota: {name}, requested: {}, used: {}, limited: {}", format_usage(&exceeded_requested), format_usage(&exceeded_used), format_usage(&exceeded_hard)))
}

/// The full decision for one pod `CREATE`: `existing_pods` is every
/// other `Pod` already in the namespace (`server::rest::list`'s own
/// output, before this new one is written) — terminal ones
/// ([`counts_toward_quota`]) are excluded from the summed usage, same as
/// real upstream. `resource_quotas` is every `ResourceQuota` in the
/// namespace; only ones [`quota_applies`] to are actually checked.
/// Returns the first quota's own denial message, real upstream's own
/// "the first `ResourceQuota` that would be exceeded wins" posture (its
/// own loop returns on the first failure, doesn't aggregate every
/// quota's own violations the way `LimitRanger`'s per-`LimitRange`
/// checks do).
pub fn check_pod_create(pod: &Value, existing_pods: &[Value], resource_quotas: &[Value]) -> Option<String> {
    let this_pod_usage = pod_usage(pod);

    let mut existing_usage = BTreeMap::new();
    for existing in existing_pods {
        if counts_toward_quota(existing) {
            existing_usage = add_maps(&existing_usage, &pod_usage(existing));
        }
    }

    for resource_quota in resource_quotas {
        if !quota_applies(resource_quota) {
            continue;
        }
        if let Some(message) = check_quota(resource_quota, &existing_usage, &this_pod_usage) {
            return Some(message);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::admission::attributes::Operation;

    fn pod_with_cpu_request(name: &str, cpu: &str) -> Value {
        json!({"metadata": {"name": name}, "spec": {"containers": [{"name": "c1", "resources": {"requests": {"cpu": cpu}}}]}})
    }

    fn quota(name: &str, hard: Value) -> Value {
        json!({"metadata": {"name": name}, "spec": {"hard": hard}})
    }

    #[test]
    fn applies_to_pod_create_only() {
        assert!(applies_to(Operation::Create, "", "pods", ""));
        assert!(!applies_to(Operation::Update, "", "pods", ""));
        assert!(!applies_to(Operation::Create, "", "pods", "status"));
        assert!(!applies_to(Operation::Create, "apps", "deployments", ""));
    }

    #[test]
    fn counts_toward_quota_excludes_terminal_pods() {
        assert!(!counts_toward_quota(&json!({"status": {"phase": "Succeeded"}})));
        assert!(!counts_toward_quota(&json!({"status": {"phase": "Failed"}})));
        assert!(counts_toward_quota(&json!({"status": {"phase": "Running"}})));
        assert!(counts_toward_quota(&json!({})), "no status at all (a just-created pod) counts");
    }

    #[test]
    fn pod_usage_always_counts_the_pod_object() {
        let usage = pod_usage(&json!({"spec": {"containers": []}}));
        assert_eq!(usage["pods"].value(), 1);
    }

    #[test]
    fn pod_usage_tracks_cpu_and_memory_requests_and_limits() {
        let pod = json!({"spec": {"containers": [{"name": "c1", "resources": {
            "requests": {"cpu": "100m", "memory": "128Mi"},
            "limits": {"cpu": "200m", "memory": "256Mi"},
        }}]}});
        let usage = pod_usage(&pod);
        assert_eq!(usage["cpu"].milli_value(), 100);
        assert_eq!(usage["requests.cpu"].milli_value(), 100);
        assert_eq!(usage["limits.cpu"].milli_value(), 200);
        assert_eq!(usage["memory"].value(), 128 * 1024 * 1024);
        assert_eq!(usage["limits.memory"].value(), 256 * 1024 * 1024);
    }

    #[test]
    fn a_pod_within_quota_is_allowed() {
        let pod = pod_with_cpu_request("new", "500m");
        let q = quota("compute-quota", json!({"requests.cpu": "1"}));
        assert!(check_pod_create(&pod, &[], &[q]).is_none());
    }

    #[test]
    fn a_pod_that_would_exceed_quota_is_denied() {
        let pod = pod_with_cpu_request("new", "600m");
        let existing = pod_with_cpu_request("existing", "500m");
        let q = quota("compute-quota", json!({"requests.cpu": "1"}));
        let denial = check_pod_create(&pod, &[existing], &[q]).expect("600m + 500m > 1 core");
        assert!(denial.contains("exceeded quota: compute-quota"));
        assert!(denial.contains("requests.cpu"));
    }

    #[test]
    fn a_terminal_existing_pod_does_not_count_against_quota() {
        let pod = pod_with_cpu_request("new", "600m");
        let mut existing = pod_with_cpu_request("existing", "500m");
        existing["status"] = json!({"phase": "Succeeded"});
        let q = quota("compute-quota", json!({"requests.cpu": "1"}));
        assert!(check_pod_create(&pod, &[existing], &[q]).is_none(), "a Succeeded pod must not count against quota");
    }

    #[test]
    fn a_quota_tracking_an_unrelated_resource_is_not_consulted() {
        let pod = pod_with_cpu_request("new", "999");
        let q = quota("services-quota", json!({"count/services": "5"}));
        assert!(check_pod_create(&pod, &[], &[q]).is_none(), "a quota tracking only count/services should never see a pod-only check");
    }

    #[test]
    fn the_pods_count_limit_is_enforced_too() {
        let pod = pod_with_cpu_request("new", "1m");
        let existing = pod_with_cpu_request("existing", "1m");
        let q = quota("pod-count-quota", json!({"pods": "1"}));
        let denial = check_pod_create(&pod, &[existing], &[q]).expect("2 pods > hard limit of 1");
        assert!(denial.contains("pods="));
    }

    #[test]
    fn the_first_exceeded_quota_wins_not_an_aggregate_of_all() {
        let pod = pod_with_cpu_request("new", "999");
        let q1 = quota("q1", json!({"requests.cpu": "1"}));
        let q2 = quota("q2", json!({"requests.cpu": "1"}));
        let denial = check_pod_create(&pod, &[], &[q1, q2]).unwrap();
        assert!(denial.contains("q1"), "the first quota in the list should be the one reported");
    }
}
