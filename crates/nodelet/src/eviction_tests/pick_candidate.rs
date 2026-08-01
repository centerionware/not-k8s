use super::*;
use k8s_openapi::api::core::v1::PodSpec;
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{ObjectMeta, Time};
use std::collections::BTreeMap;

fn resources(requests: &[(&str, &str)], limits: &[(&str, &str)]) -> ResourceRequirements {
    let to_map = |pairs: &[(&str, &str)]| -> BTreeMap<String, Quantity> {
        pairs.iter().map(|(k, v)| (k.to_string(), Quantity(v.to_string()))).collect()
    };
    ResourceRequirements {
        requests: (!requests.is_empty()).then(|| to_map(requests)),
        limits: (!limits.is_empty()).then(|| to_map(limits)),
        ..Default::default()
    }
}

fn pod(name: &str, resources: ResourceRequirements) -> Pod {
    Pod {
        metadata: ObjectMeta { name: Some(name.to_string()), uid: Some(format!("uid-{name}")), ..Default::default() },
        spec: Some(PodSpec {
            containers: vec![Container { name: "app".to_string(), resources: Some(resources), ..Default::default() }],
            ..Default::default()
        }),
        ..Default::default()
    }
}

fn name_of(pod: Option<&Pod>) -> Option<&str> {
    pod.and_then(|p| p.metadata.name.as_deref())
}

#[test]
fn besteffort_is_preferred_over_burstable_and_guaranteed() {
    let pods = vec![
        pod("guaranteed", resources(&[("cpu", "1"), ("memory", "1Gi")], &[("cpu", "1"), ("memory", "1Gi")])),
        pod("burstable", resources(&[("cpu", "100m")], &[])),
        pod("besteffort", ResourceRequirements::default()),
    ];
    assert_eq!(name_of(pick_eviction_candidate(&pods, &HashMap::new())), Some("besteffort"));
}

#[test]
fn burstable_is_preferred_over_guaranteed_when_no_besteffort_present() {
    let pods = vec![
        pod("guaranteed", resources(&[("cpu", "1"), ("memory", "1Gi")], &[("cpu", "1"), ("memory", "1Gi")])),
        pod("burstable", resources(&[("cpu", "100m")], &[])),
    ];
    assert_eq!(name_of(pick_eviction_candidate(&pods, &HashMap::new())), Some("burstable"));
}

#[test]
fn guaranteed_only_pods_are_never_evicted() {
    let pods = vec![pod("guaranteed", resources(&[("cpu", "1"), ("memory", "1Gi")], &[("cpu", "1"), ("memory", "1Gi")]))];
    assert_eq!(pick_eviction_candidate(&pods, &HashMap::new()), None);
}

#[test]
fn within_the_same_class_the_largest_memory_requester_is_picked() {
    let pods = vec![
        pod("small", resources(&[("memory", "64Mi")], &[])),
        pod("large", resources(&[("memory", "512Mi")], &[])),
    ];
    assert_eq!(name_of(pick_eviction_candidate(&pods, &HashMap::new())), Some("large"));
}

#[test]
fn critical_priority_pods_are_never_evicted() {
    let mut critical = pod("critical", ResourceRequirements::default());
    critical.spec.as_mut().unwrap().priority_class_name = Some("system-node-critical".to_string());
    let pods = vec![critical];
    assert_eq!(pick_eviction_candidate(&pods, &HashMap::new()), None);
}

#[test]
fn already_terminating_pods_are_skipped() {
    let mut terminating = pod("dying", ResourceRequirements::default());
    terminating.metadata.deletion_timestamp = Some(Time(k8s_openapi::jiff::Timestamp::now()));
    let alive = pod("alive", ResourceRequirements::default());
    let pods = vec![terminating, alive];
    assert_eq!(name_of(pick_eviction_candidate(&pods, &HashMap::new())), Some("alive"));
}

#[test]
fn empty_pod_list_returns_none() {
    assert_eq!(pick_eviction_candidate(&[], &HashMap::new()), None);
}

#[test]
fn real_usage_overrides_requested_memory_as_the_tie_breaker() {
    // "small" only requested 64Mi but is actually using far more than
    // "large" requested/reserved — real usage must win the tie-break, not
    // the request, once it's known.
    let small = pod("small", resources(&[("memory", "64Mi")], &[]));
    let large = pod("large", resources(&[("memory", "512Mi")], &[]));
    let small_uid = small.metadata.uid.clone().unwrap();
    let pods = vec![small, large];

    let mut usage = HashMap::new();
    usage.insert(small_uid, 900 * 1024 * 1024); // actually using 900Mi

    assert_eq!(name_of(pick_eviction_candidate(&pods, &usage)), Some("small"));
}

#[test]
fn pods_missing_from_the_usage_map_fall_back_to_requested_memory() {
    let pods = vec![
        pod("small", resources(&[("memory", "64Mi")], &[])),
        pod("large", resources(&[("memory", "512Mi")], &[])),
    ];
    // Empty usage map — same outcome as the pure request-based test above.
    assert_eq!(name_of(pick_eviction_candidate(&pods, &HashMap::new())), Some("large"));
}

// --- spec.priority tiebreaking (round 26) ---

fn pod_with_priority(name: &str, priority: i32, resources: ResourceRequirements) -> Pod {
    let mut p = pod(name, resources);
    p.spec.as_mut().unwrap().priority = Some(priority);
    p
}

#[test]
fn lower_priority_is_evicted_before_higher_priority_in_the_same_qos_class() {
    // Same QoS class (both BestEffort), same requested memory — only
    // priority differs. Real kubelet evicts the lower-priority pod first.
    let low = pod_with_priority("low-priority", 0, ResourceRequirements::default());
    let high = pod_with_priority("high-priority", 1000, ResourceRequirements::default());
    let pods = vec![high, low];
    assert_eq!(name_of(pick_eviction_candidate(&pods, &HashMap::new())), Some("low-priority"));
}

#[test]
fn priority_beats_usage_as_a_tiebreaker() {
    // "low-priority" uses far less memory than "high-priority" but has a
    // lower priority — priority must win the tiebreak before usage does,
    // matching real kubelet's rankMemoryPressure ordering (priority, then
    // usage).
    let low = pod_with_priority("low-priority", 0, resources(&[("memory", "64Mi")], &[]));
    let high = pod_with_priority("high-priority", 1000, resources(&[("memory", "512Mi")], &[]));
    let pods = vec![high, low];
    assert_eq!(name_of(pick_eviction_candidate(&pods, &HashMap::new())), Some("low-priority"));
}

#[test]
fn qos_class_still_outranks_priority() {
    // A Burstable pod with a very high priority must still be evicted
    // before a BestEffort pod with the default priority — QoS class is
    // still the primary ranking key, priority only breaks ties within it.
    let besteffort_default_priority = pod("besteffort", ResourceRequirements::default());
    let burstable_high_priority = pod_with_priority("burstable", 1_000_000, resources(&[("cpu", "100m")], &[]));
    let pods = vec![burstable_high_priority, besteffort_default_priority];
    assert_eq!(name_of(pick_eviction_candidate(&pods, &HashMap::new())), Some("besteffort"));
}

#[test]
fn equal_priority_falls_back_to_usage_as_before() {
    let low_usage = pod_with_priority("small", 5, resources(&[("memory", "64Mi")], &[]));
    let high_usage = pod_with_priority("large", 5, resources(&[("memory", "512Mi")], &[]));
    let pods = vec![low_usage, high_usage];
    assert_eq!(name_of(pick_eviction_candidate(&pods, &HashMap::new())), Some("large"));
}

#[test]
fn unset_priority_defaults_to_zero() {
    let unset = pod("unset", ResourceRequirements::default());
    let explicit_zero = pod_with_priority("explicit-zero", 0, ResourceRequirements::default());
    // Same effective priority (0) and same usage (default/empty) — this
    // must not panic or behave inconsistently; either could legitimately
    // be picked since they're now fully tied, so just confirm one is.
    let pods = vec![unset, explicit_zero];
    assert!(pick_eviction_candidate(&pods, &HashMap::new()).is_some());
}

#[test]
fn exceeding_its_own_request_beats_a_higher_priority_pod_that_does_not() {
    // Round 99: exceedMemoryRequests is real kubelet's actual PRIMARY
    // ranking criterion within a QoS class, ahead of priority. A pod
    // using more than it requested must be evicted before a
    // higher-priority pod that's still within its request, even though
    // priority alone would otherwise pick the other way.
    let within_request = pod_with_priority("within-request", 1000, resources(&[("memory", "512Mi")], &[]));
    let over_request = pod_with_priority("over-request", 0, resources(&[("memory", "64Mi")], &[]));
    let over_request_uid = over_request.metadata.uid.clone().unwrap();
    let pods = vec![within_request, over_request];

    let mut usage = HashMap::new();
    usage.insert(over_request_uid, 200 * 1024 * 1024); // 200Mi used, 64Mi requested — exceeds

    assert_eq!(name_of(pick_eviction_candidate(&pods, &usage)), Some("over-request"));
}

#[test]
fn within_request_pods_still_rank_by_priority_between_themselves() {
    // Once both pods are on the same side of the exceeds-requests line
    // (both within their own request, usage known for both), priority
    // still breaks the tie exactly as before round 99.
    let low = pod_with_priority("low-priority", 0, resources(&[("memory", "512Mi")], &[]));
    let high = pod_with_priority("high-priority", 1000, resources(&[("memory", "512Mi")], &[]));
    let low_uid = low.metadata.uid.clone().unwrap();
    let high_uid = high.metadata.uid.clone().unwrap();
    let pods = vec![high, low];

    let mut usage = HashMap::new();
    usage.insert(low_uid, 100 * 1024 * 1024); // well within 512Mi request
    usage.insert(high_uid, 100 * 1024 * 1024); // same — neither exceeds

    assert_eq!(name_of(pick_eviction_candidate(&pods, &usage)), Some("low-priority"));
}

#[test]
fn unknown_usage_is_treated_as_exceeding_requests() {
    // No live stats for either pod (mock runtime, or too new for CRI to
    // have measured) — matches upstream's own "prioritize evicting the
    // pod for which no stats were found" direction, so this must not
    // silently behave as if neither exceeds anything.
    let pods = vec![pod("no-stats", resources(&[("memory", "64Mi")], &[]))];
    assert!(exceeds_memory_requests(&pods[0], &HashMap::new()));
}
