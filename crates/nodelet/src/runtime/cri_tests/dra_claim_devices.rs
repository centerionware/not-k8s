//! resource_claim_object_name()/allocated_devices_by_driver()/
//! cdi_devices_for_container_claim(): the pure translation logic behind
//! Dynamic Resource Allocation (round 63) — resolving which ResourceClaim
//! object a pod-claim points at, grouping its allocated devices by
//! driver, and picking which CDI device IDs a specific container's
//! `resources.claims[]` entry should get.
use super::*;
use k8s_openapi::api::core::v1::PodResourceClaimStatus;
use k8s_openapi::api::resource::v1beta1::{
    AllocationResult, DeviceAllocationResult, DeviceRequestAllocationResult, ResourceClaimSpec, ResourceClaimStatus,
};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;

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

fn claim_with_results(results: Vec<DeviceRequestAllocationResult>) -> DraResourceClaim {
    DraResourceClaim {
        metadata: ObjectMeta::default(),
        spec: ResourceClaimSpec::default(),
        status: Some(ResourceClaimStatus {
            allocation: Some(AllocationResult {
                devices: Some(DeviceAllocationResult { config: None, results: Some(results) }),
                node_selector: None,
            }),
            devices: None,
            reserved_for: None,
        }),
    }
}

fn result(driver: &str, request: &str, device: &str) -> DeviceRequestAllocationResult {
    DeviceRequestAllocationResult {
        admin_access: None,
        device: device.to_string(),
        driver: driver.to_string(),
        pool: "default".to_string(),
        request: request.to_string(),
        tolerations: None,
    }
}

#[test]
fn unallocated_claim_groups_to_an_empty_map() {
    let claim = DraResourceClaim { metadata: ObjectMeta::default(), spec: ResourceClaimSpec::default(), status: None };
    assert!(allocated_devices_by_driver(&claim).is_empty());
}

#[test]
fn single_driver_single_device() {
    let claim = claim_with_results(vec![result("gpu.example.com", "req-a", "gpu-0")]);
    let by_driver = allocated_devices_by_driver(&claim);
    assert_eq!(by_driver.len(), 1);
    assert_eq!(by_driver.get("gpu.example.com").unwrap().len(), 1);
}

#[test]
fn multiple_devices_from_different_drivers_are_split() {
    let claim = claim_with_results(vec![result("gpu.example.com", "req-a", "gpu-0"), result("nic.example.com", "req-b", "nic-0")]);
    let by_driver = allocated_devices_by_driver(&claim);
    assert_eq!(by_driver.len(), 2);
    assert!(by_driver.contains_key("gpu.example.com"));
    assert!(by_driver.contains_key("nic.example.com"));
}

#[test]
fn multiple_devices_from_the_same_driver_stay_grouped_together() {
    let claim = claim_with_results(vec![result("gpu.example.com", "req-a", "gpu-0"), result("gpu.example.com", "req-b", "gpu-1")]);
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
