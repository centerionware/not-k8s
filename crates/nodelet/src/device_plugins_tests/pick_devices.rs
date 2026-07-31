use super::*;

fn dev(id: &str, healthy: bool) -> DeviceInfo {
    DeviceInfo { id: id.to_string(), healthy }
}

#[test]
fn picks_the_first_n_healthy_devices() {
    let devices = vec![dev("gpu-0", true), dev("gpu-1", true), dev("gpu-2", true)];
    let picked = pick_devices(&devices, &HashSet::new(), 2).unwrap();
    assert_eq!(picked, vec!["gpu-0".to_string(), "gpu-1".to_string()]);
}

#[test]
fn unhealthy_devices_are_never_picked() {
    let devices = vec![dev("gpu-0", false), dev("gpu-1", true)];
    let picked = pick_devices(&devices, &HashSet::new(), 1).unwrap();
    assert_eq!(picked, vec!["gpu-1".to_string()]);
}

#[test]
fn already_allocated_devices_are_skipped() {
    let devices = vec![dev("gpu-0", true), dev("gpu-1", true)];
    let mut allocated = HashSet::new();
    allocated.insert("gpu-0".to_string());
    let picked = pick_devices(&devices, &allocated, 1).unwrap();
    assert_eq!(picked, vec!["gpu-1".to_string()]);
}

#[test]
fn zero_count_returns_an_empty_pick_not_none() {
    let devices = vec![dev("gpu-0", true)];
    let picked = pick_devices(&devices, &HashSet::new(), 0).unwrap();
    assert!(picked.is_empty());
}

#[test]
fn not_enough_healthy_unallocated_devices_returns_none() {
    let devices = vec![dev("gpu-0", true), dev("gpu-1", false)];
    assert!(pick_devices(&devices, &HashSet::new(), 2).is_none());
}

#[test]
fn empty_device_list_with_nonzero_count_returns_none() {
    assert!(pick_devices(&[], &HashSet::new(), 1).is_none());
}
