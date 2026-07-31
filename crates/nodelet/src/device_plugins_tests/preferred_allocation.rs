//! is_valid_preferred_allocation(): whether a GetPreferredAllocation
//! response is safe to trust and use verbatim, or whether
//! allocate_preferring() should fall back to nodelet's own
//! pick_devices_preferring() selection instead.
use super::*;

fn dev(id: &str, healthy: bool) -> DeviceInfo {
    DeviceInfo { id: id.to_string(), healthy, numa_node: None }
}

fn ids(vals: &[&str]) -> Vec<String> {
    vals.iter().map(|s| s.to_string()).collect()
}

#[test]
fn a_valid_response_is_accepted() {
    let devices = vec![dev("gpu-0", true), dev("gpu-1", true)];
    assert!(is_valid_preferred_allocation(&ids(&["gpu-0"]), &devices, &HashSet::new(), 1));
}

#[test]
fn wrong_count_is_rejected() {
    let devices = vec![dev("gpu-0", true), dev("gpu-1", true)];
    assert!(!is_valid_preferred_allocation(&ids(&["gpu-0"]), &devices, &HashSet::new(), 2));
    assert!(!is_valid_preferred_allocation(&ids(&["gpu-0", "gpu-1"]), &devices, &HashSet::new(), 1));
}

#[test]
fn duplicate_ids_are_rejected() {
    let devices = vec![dev("gpu-0", true), dev("gpu-1", true)];
    assert!(!is_valid_preferred_allocation(&ids(&["gpu-0", "gpu-0"]), &devices, &HashSet::new(), 2));
}

#[test]
fn an_id_the_plugin_never_reported_is_rejected() {
    let devices = vec![dev("gpu-0", true)];
    assert!(!is_valid_preferred_allocation(&ids(&["ghost-device"]), &devices, &HashSet::new(), 1));
}

#[test]
fn an_unhealthy_device_is_rejected() {
    let devices = vec![dev("gpu-0", false)];
    assert!(!is_valid_preferred_allocation(&ids(&["gpu-0"]), &devices, &HashSet::new(), 1));
}

#[test]
fn an_already_allocated_device_is_rejected() {
    let devices = vec![dev("gpu-0", true)];
    let mut allocated = HashSet::new();
    allocated.insert("gpu-0".to_string());
    assert!(!is_valid_preferred_allocation(&ids(&["gpu-0"]), &devices, &allocated, 1));
}

#[test]
fn an_empty_response_for_a_zero_count_request_is_valid() {
    assert!(is_valid_preferred_allocation(&[], &[], &HashSet::new(), 0));
}
