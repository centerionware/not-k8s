//! resource_claim_object_name()/allocated_devices_by_driver()/
//! cdi_devices_for_container_claim(): the pure translation logic behind
//! Dynamic Resource Allocation (round 63) — resolving which ResourceClaim
//! object a pod-claim points at, grouping its allocated devices by
//! driver, and picking which CDI device IDs a specific container's
//! `resources.claims[]` entry should get. Uses the hand-written
//! `RawResourceClaim` family (round 121 — see that type's own doc
//! comment) rather than a k8s-openapi-generated type.
use super::*;
use k8s_openapi::api::core::v1::PodResourceClaimStatus;

// --- resource_claim_object_name() ---

#[test]
fn direct_resource_claim_name_is_a_pure_pass_through() {
    let name = resource_claim_object_name("pod-claim", Some("my-claim"), None);
    assert_eq!(name, Some("my-claim".to_string()));
}

#[test]
fn template_based_claim_resolves_via_matching_status_entry() {
    let statuses = vec![PodResourceClaimStatus { name: "pod-claim".to_string(), resource_claim_name: Some("generated-abc123".to_string()) }];
    let name = resource_claim_object_name("pod-claim", None, Some(&statuses));
    assert_eq!(name, Some("generated-abc123".to_string()));
}

#[test]
fn template_based_claim_not_yet_in_status_resolves_to_none() {
    let name = resource_claim_object_name("pod-claim", None, None);
    assert_eq!(name, None);
    let statuses = vec![PodResourceClaimStatus { name: "other-claim".to_string(), resource_claim_name: Some("generated".to_string()) }];
    assert_eq!(resource_claim_object_name("pod-claim", None, Some(&statuses)), None);
}

#[test]
fn direct_name_wins_even_if_status_entries_are_present() {
    let statuses = vec![PodResourceClaimStatus { name: "pod-claim".to_string(), resource_claim_name: Some("generated".to_string()) }];
    let name = resource_claim_object_name("pod-claim", Some("direct-claim"), Some(&statuses));
    assert_eq!(name, Some("direct-claim".to_string()));
}

// --- allocated_devices_by_driver() ---

fn claim_with_results(results: Vec<RawDeviceRequestAllocationResult>) -> RawResourceClaim {
    RawResourceClaim {
        metadata: RawObjectMeta::default(),
        status: Some(RawResourceClaimStatus {
            allocation: Some(RawAllocationResult { devices: Some(RawDeviceAllocationResult { results: Some(results) }) }),
            reserved_for: None,
        }),
    }
}

fn result(driver: &str) -> RawDeviceRequestAllocationResult {
    RawDeviceRequestAllocationResult { driver: driver.to_string() }
}

#[test]
fn unallocated_claim_groups_to_an_empty_map() {
    let claim = RawResourceClaim::default();
    assert!(allocated_devices_by_driver(&claim).is_empty());
}

#[test]
fn single_driver_single_device() {
    let claim = claim_with_results(vec![result("gpu.example.com")]);
    let by_driver = allocated_devices_by_driver(&claim);
    assert_eq!(by_driver.len(), 1);
    assert_eq!(by_driver.get("gpu.example.com").unwrap().len(), 1);
}

#[test]
fn multiple_devices_from_different_drivers_are_split() {
    let claim = claim_with_results(vec![result("gpu.example.com"), result("nic.example.com")]);
    let by_driver = allocated_devices_by_driver(&claim);
    assert_eq!(by_driver.len(), 2);
    assert!(by_driver.contains_key("gpu.example.com"));
    assert!(by_driver.contains_key("nic.example.com"));
}

#[test]
fn multiple_devices_from_the_same_driver_stay_grouped_together() {
    let claim = claim_with_results(vec![result("gpu.example.com"), result("gpu.example.com")]);
    let by_driver = allocated_devices_by_driver(&claim);
    assert_eq!(by_driver.len(), 1);
    assert_eq!(by_driver.get("gpu.example.com").unwrap().len(), 2);
}

// --- cdi_devices_for_container_claim() ---

fn prepared(all: &[&str], by_request: &[(&str, &[&str])]) -> PreparedPodClaim {
    PreparedPodClaim {
        all: all.iter().map(|s| s.to_string()).collect(),
        by_request: by_request.iter().map(|(k, v)| (k.to_string(), v.iter().map(|s| s.to_string()).collect())).collect(),
    }
}

#[test]
fn unknown_pod_claim_name_yields_no_devices() {
    let prepared_map = HashMap::new();
    assert!(cdi_devices_for_container_claim("missing", None, &prepared_map).is_empty());
}

#[test]
fn no_request_filter_returns_every_device_in_the_claim() {
    let mut map = HashMap::new();
    map.insert("pod-claim".to_string(), prepared(&["vendor.com/gpu=0", "vendor.com/gpu=1"], &[("req-a", &["vendor.com/gpu=0"]), ("req-b", &["vendor.com/gpu=1"])]));
    let devices = cdi_devices_for_container_claim("pod-claim", None, &map);
    assert_eq!(devices.len(), 2);
}

#[test]
fn a_specific_request_filters_to_just_that_requests_devices() {
    let mut map = HashMap::new();
    map.insert("pod-claim".to_string(), prepared(&["vendor.com/gpu=0", "vendor.com/gpu=1"], &[("req-a", &["vendor.com/gpu=0"]), ("req-b", &["vendor.com/gpu=1"])]));
    let devices = cdi_devices_for_container_claim("pod-claim", Some("req-a"), &map);
    assert_eq!(devices, vec!["vendor.com/gpu=0".to_string()]);
}

#[test]
fn a_subrequest_match_uses_the_main_request_slash_subrequest_prefix() {
    let mut map = HashMap::new();
    map.insert("pod-claim".to_string(), prepared(&["vendor.com/gpu=0"], &[("req-a/sub-1", &["vendor.com/gpu=0"])]));
    let devices = cdi_devices_for_container_claim("pod-claim", Some("req-a"), &map);
    assert_eq!(devices, vec!["vendor.com/gpu=0".to_string()]);
}

#[test]
fn a_request_name_with_no_matching_devices_yields_an_empty_list() {
    let mut map = HashMap::new();
    map.insert("pod-claim".to_string(), prepared(&["vendor.com/gpu=0"], &[("req-a", &["vendor.com/gpu=0"])]));
    assert!(cdi_devices_for_container_claim("pod-claim", Some("req-nonexistent"), &map).is_empty());
}

// --- pod_is_reserved_for_claim() (round 64) ---

fn claim_reserved_for(refs: Vec<RawConsumerReference>) -> RawResourceClaim {
    RawResourceClaim { metadata: RawObjectMeta::default(), status: Some(RawResourceClaimStatus { allocation: None, reserved_for: Some(refs) }) }
}

fn consumer_ref(name: &str, uid: &str) -> RawConsumerReference {
    RawConsumerReference { name: name.to_string(), resource: "pods".to_string(), uid: uid.to_string() }
}

#[test]
fn no_status_at_all_is_not_reserved() {
    let claim = RawResourceClaim::default();
    assert!(!pod_is_reserved_for_claim(&claim, "my-pod", "uid-1"));
}

#[test]
fn no_reserved_for_entries_is_not_reserved() {
    let claim = claim_reserved_for(vec![]);
    assert!(!pod_is_reserved_for_claim(&claim, "my-pod", "uid-1"));
}

#[test]
fn a_matching_pod_name_and_uid_is_reserved() {
    let claim = claim_reserved_for(vec![consumer_ref("my-pod", "uid-1")]);
    assert!(pod_is_reserved_for_claim(&claim, "my-pod", "uid-1"));
}

#[test]
fn a_different_pods_reservation_does_not_count() {
    let claim = claim_reserved_for(vec![consumer_ref("other-pod", "uid-2")]);
    assert!(!pod_is_reserved_for_claim(&claim, "my-pod", "uid-1"));
}

#[test]
fn matching_name_but_different_uid_does_not_count() {
    // UID is what actually disambiguates two incarnations of the same
    // pod name (e.g. after a delete+recreate) — matching on name alone
    // would be a real correctness bug.
    let claim = claim_reserved_for(vec![consumer_ref("my-pod", "stale-uid")]);
    assert!(!pod_is_reserved_for_claim(&claim, "my-pod", "uid-1"));
}

#[test]
fn one_matching_entry_among_several_others_is_reserved() {
    let claim = claim_reserved_for(vec![consumer_ref("other-pod", "uid-2"), consumer_ref("my-pod", "uid-1")]);
    assert!(pod_is_reserved_for_claim(&claim, "my-pod", "uid-1"));
}
