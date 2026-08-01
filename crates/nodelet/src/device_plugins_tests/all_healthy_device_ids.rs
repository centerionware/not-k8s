//! all_healthy_device_ids(): the PodResources API's GetAllocatableResources
//! (round 74) needs the actual IDs, not just capacity_map()'s counts.
use super::*;

fn plugins_with(entries: Vec<(&str, Vec<DeviceInfo>)>) -> DevicePlugins {
    let mut plugins = HashMap::new();
    for (name, devices) in entries {
        plugins.insert(
            name.to_string(),
            PluginState {
                endpoint: "unix:///dummy.sock".to_string(),
                devices,
                allocated: HashSet::new(),
                pre_start_required: false,
                get_preferred_allocation_available: false,
            },
        );
    }
    DevicePlugins { plugins: Mutex::new(plugins) }
}

fn dev(id: &str, healthy: bool) -> DeviceInfo {
    DeviceInfo { id: id.to_string(), healthy, numa_node: None }
}

#[test]
fn no_plugins_registered_gives_an_empty_list() {
    let dp = DevicePlugins::new();
    assert!(dp.all_healthy_device_ids().is_empty());
}

#[test]
fn only_healthy_device_ids_are_included() {
    let dp = plugins_with(vec![("nvidia.com/gpu", vec![dev("gpu-0", true), dev("gpu-1", false), dev("gpu-2", true)])]);
    let ids = dp.all_healthy_device_ids();
    assert_eq!(ids.len(), 1);
    let (name, healthy_ids) = &ids[0];
    assert_eq!(name, "nvidia.com/gpu");
    assert_eq!(healthy_ids, &vec!["gpu-0".to_string(), "gpu-2".to_string()]);
}

#[test]
fn a_resource_with_no_healthy_devices_still_appears_with_an_empty_list() {
    let dp = plugins_with(vec![("nvidia.com/gpu", vec![dev("gpu-0", false)])]);
    let ids = dp.all_healthy_device_ids();
    assert_eq!(ids.len(), 1);
    assert!(ids[0].1.is_empty());
}

#[test]
fn allocated_devices_are_still_reported_here_unlike_pick_devices() {
    // all_healthy_device_ids() reports the whole allocatable pool
    // regardless of current allocation, same "total capacity, not just
    // what's free right now" semantics as cpu_manager's
    // allocatable_cpus() -- allocation accounting is the scheduler's job.
    let dp = plugins_with(vec![("nvidia.com/gpu", vec![dev("gpu-0", true)])]);
    dp.plugins.lock().unwrap().get_mut("nvidia.com/gpu").unwrap().allocated.insert("gpu-0".to_string());
    let ids = dp.all_healthy_device_ids();
    assert_eq!(ids[0].1, vec!["gpu-0".to_string()]);
}
