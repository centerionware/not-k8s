use super::*;

fn dev(id: &str, healthy: bool) -> DeviceInfo {
    DeviceInfo { id: id.to_string(), healthy, numa_node: None }
}

fn dev_on(id: &str, healthy: bool, numa_node: u32) -> DeviceInfo {
    DeviceInfo { id: id.to_string(), healthy, numa_node: Some(numa_node) }
}

#[test]
fn picks_the_first_n_healthy_devices() {
    let devices = vec![dev("gpu-0", true), dev("gpu-1", true), dev("gpu-2", true)];
    let picked = pick_devices_preferring(&devices, &HashSet::new(), 2, None).unwrap();
    assert_eq!(picked, vec!["gpu-0".to_string(), "gpu-1".to_string()]);
}

#[test]
fn unhealthy_devices_are_never_picked() {
    let devices = vec![dev("gpu-0", false), dev("gpu-1", true)];
    let picked = pick_devices_preferring(&devices, &HashSet::new(), 1, None).unwrap();
    assert_eq!(picked, vec!["gpu-1".to_string()]);
}

#[test]
fn already_allocated_devices_are_skipped() {
    let devices = vec![dev("gpu-0", true), dev("gpu-1", true)];
    let mut allocated = HashSet::new();
    allocated.insert("gpu-0".to_string());
    let picked = pick_devices_preferring(&devices, &allocated, 1, None).unwrap();
    assert_eq!(picked, vec!["gpu-1".to_string()]);
}

#[test]
fn zero_count_returns_an_empty_pick_not_none() {
    let devices = vec![dev("gpu-0", true)];
    let picked = pick_devices_preferring(&devices, &HashSet::new(), 0, None).unwrap();
    assert!(picked.is_empty());
}

#[test]
fn not_enough_healthy_unallocated_devices_returns_none() {
    let devices = vec![dev("gpu-0", true), dev("gpu-1", false)];
    assert!(pick_devices_preferring(&devices, &HashSet::new(), 2, None).is_none());
}

#[test]
fn empty_device_list_with_nonzero_count_returns_none() {
    assert!(pick_devices_preferring(&[], &HashSet::new(), 1, None).is_none());
}

#[test]
fn preferred_numa_node_devices_are_picked_first() {
    let devices = vec![dev_on("gpu-0", true, 0), dev_on("gpu-1", true, 1)];
    let picked = pick_devices_preferring(&devices, &HashSet::new(), 1, Some(1)).unwrap();
    assert_eq!(picked, vec!["gpu-1".to_string()]);
}

#[test]
fn untagged_devices_count_toward_the_preferred_node_pass() {
    let devices = vec![dev("gpu-0", true), dev_on("gpu-1", true, 5)];
    let picked = pick_devices_preferring(&devices, &HashSet::new(), 1, Some(5)).unwrap();
    // Either satisfies the request; the untagged one comes first in list order.
    assert_eq!(picked, vec!["gpu-0".to_string()]);
}

#[test]
fn falls_back_to_other_nodes_when_the_preferred_node_cannot_supply_enough() {
    let devices = vec![dev_on("gpu-0", true, 0), dev_on("gpu-1", true, 1)];
    let picked = pick_devices_preferring(&devices, &HashSet::new(), 2, Some(0)).unwrap();
    let mut picked = picked;
    picked.sort();
    assert_eq!(picked, vec!["gpu-0".to_string(), "gpu-1".to_string()]);
}

#[test]
fn preferred_node_with_no_matching_devices_still_falls_back() {
    let devices = vec![dev_on("gpu-0", true, 7)];
    let picked = pick_devices_preferring(&devices, &HashSet::new(), 1, Some(0)).unwrap();
    assert_eq!(picked, vec!["gpu-0".to_string()]);
}
