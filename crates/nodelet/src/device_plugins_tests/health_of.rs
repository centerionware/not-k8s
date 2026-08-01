//! health_of(): live per-device health for allocatedResourcesStatus
//! (round 79; ResourceHealthStatus, found in round 72's re-audit).
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
fn returns_the_live_health_of_a_known_device() {
    let dp = plugins_with(vec![("nvidia.com/gpu", vec![dev("gpu-0", true), dev("gpu-1", false)])]);
    assert_eq!(dp.health_of("nvidia.com/gpu", "gpu-0"), Some(true));
    assert_eq!(dp.health_of("nvidia.com/gpu", "gpu-1"), Some(false));
}

#[test]
fn none_for_an_unregistered_resource() {
    let dp = DevicePlugins::new();
    assert_eq!(dp.health_of("nvidia.com/gpu", "gpu-0"), None);
}

#[test]
fn none_for_a_device_id_not_in_the_current_inventory() {
    let dp = plugins_with(vec![("nvidia.com/gpu", vec![dev("gpu-0", true)])]);
    assert_eq!(dp.health_of("nvidia.com/gpu", "gpu-99"), None);
}
