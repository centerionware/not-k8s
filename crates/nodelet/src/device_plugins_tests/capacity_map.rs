use super::*;

fn plugins_with(entries: Vec<(&str, Vec<DeviceInfo>)>) -> DevicePlugins {
    let mut plugins = HashMap::new();
    for (name, devices) in entries {
        plugins.insert(name.to_string(), PluginState { endpoint: "unix:///dummy.sock".to_string(), devices, allocated: HashSet::new() });
    }
    DevicePlugins { plugins: Mutex::new(plugins) }
}

fn dev(id: &str, healthy: bool) -> DeviceInfo {
    DeviceInfo { id: id.to_string(), healthy, numa_node: None }
}

#[test]
fn no_plugins_registered_gives_an_empty_map() {
    let dp = DevicePlugins::new();
    assert!(dp.capacity_map().is_empty());
}

#[test]
fn counts_only_healthy_devices() {
    let dp = plugins_with(vec![("nvidia.com/gpu", vec![dev("gpu-0", true), dev("gpu-1", false), dev("gpu-2", true)])]);
    assert_eq!(dp.capacity_map().get("nvidia.com/gpu"), Some(&2));
}

#[test]
fn a_resource_with_zero_healthy_devices_still_appears_as_zero() {
    // Not omitted — real kubelet reports 0, not "resource doesn't exist,"
    // once a plugin has registered but every device is unhealthy.
    let dp = plugins_with(vec![("nvidia.com/gpu", vec![dev("gpu-0", false)])]);
    assert_eq!(dp.capacity_map().get("nvidia.com/gpu"), Some(&0));
}

#[test]
fn multiple_resources_are_all_reported() {
    let dp = plugins_with(vec![("nvidia.com/gpu", vec![dev("gpu-0", true)]), ("example.com/fpga", vec![dev("fpga-0", true), dev("fpga-1", true)])]);
    let m = dp.capacity_map();
    assert_eq!(m.get("nvidia.com/gpu"), Some(&1));
    assert_eq!(m.get("example.com/fpga"), Some(&2));
}
