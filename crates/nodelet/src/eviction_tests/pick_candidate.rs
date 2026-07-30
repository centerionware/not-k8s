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
