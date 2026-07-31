//! register()/deregister()/resource_configured() and the internal
//! update_devices()/is_current() staleness checks the ListAndWatch watch
//! loop relies on to know when to stop after a deregistration or
//! re-registration under a different endpoint.
use super::*;

#[tokio::test]
async fn an_unregistered_resource_is_unconfigured() {
    let dp = DevicePlugins::new();
    assert!(!dp.resource_configured("nvidia.com/gpu"));
}

#[tokio::test]
async fn registering_makes_a_resource_configured() {
    let dp = Arc::new(DevicePlugins::new());
    dp.register("nvidia.com/gpu".to_string(), "unix:///does-not-exist.sock".to_string());
    assert!(dp.resource_configured("nvidia.com/gpu"));
}

#[tokio::test]
async fn deregistering_makes_it_unconfigured_again() {
    let dp = Arc::new(DevicePlugins::new());
    dp.register("nvidia.com/gpu".to_string(), "unix:///does-not-exist.sock".to_string());
    dp.deregister("nvidia.com/gpu");
    assert!(!dp.resource_configured("nvidia.com/gpu"));
}

#[tokio::test]
async fn is_current_matches_the_live_endpoint() {
    let dp = Arc::new(DevicePlugins::new());
    dp.register("nvidia.com/gpu".to_string(), "unix:///a.sock".to_string());
    assert!(dp.is_current("nvidia.com/gpu", "unix:///a.sock"));
    assert!(!dp.is_current("nvidia.com/gpu", "unix:///b.sock"));
}

#[tokio::test]
async fn deregistering_makes_every_endpoint_stale() {
    let dp = Arc::new(DevicePlugins::new());
    dp.register("nvidia.com/gpu".to_string(), "unix:///a.sock".to_string());
    dp.deregister("nvidia.com/gpu");
    assert!(!dp.is_current("nvidia.com/gpu", "unix:///a.sock"));
}

#[tokio::test]
async fn update_devices_is_rejected_for_a_stale_endpoint() {
    let dp = Arc::new(DevicePlugins::new());
    dp.register("nvidia.com/gpu".to_string(), "unix:///a.sock".to_string());
    // A different (fresher) registration replaced this one.
    dp.register("nvidia.com/gpu".to_string(), "unix:///b.sock".to_string());
    let accepted = dp.update_devices("nvidia.com/gpu", "unix:///a.sock", vec![DeviceInfo { id: "gpu-0".to_string(), healthy: true, numa_node: None }]);
    assert!(!accepted);
    // The fresher registration's (empty, since nothing streamed to it yet)
    // device list must be untouched by the stale update.
    assert_eq!(dp.capacity_map().get("nvidia.com/gpu"), Some(&0));
}

#[tokio::test]
async fn update_devices_is_accepted_for_the_current_endpoint() {
    let dp = Arc::new(DevicePlugins::new());
    dp.register("nvidia.com/gpu".to_string(), "unix:///a.sock".to_string());
    let accepted = dp.update_devices("nvidia.com/gpu", "unix:///a.sock", vec![DeviceInfo { id: "gpu-0".to_string(), healthy: true, numa_node: None }]);
    assert!(accepted);
    assert_eq!(dp.capacity_map().get("nvidia.com/gpu"), Some(&1));
}

#[tokio::test]
async fn release_frees_a_device_for_reallocation() {
    let dp = Arc::new(DevicePlugins::new());
    dp.register("nvidia.com/gpu".to_string(), "unix:///a.sock".to_string());
    dp.update_devices("nvidia.com/gpu", "unix:///a.sock", vec![DeviceInfo { id: "gpu-0".to_string(), healthy: true, numa_node: None }]);
    {
        let mut plugins = dp.plugins.lock().unwrap();
        plugins.get_mut("nvidia.com/gpu").unwrap().allocated.insert("gpu-0".to_string());
    }
    dp.release("nvidia.com/gpu", &["gpu-0".to_string()]);
    let plugins = dp.plugins.lock().unwrap();
    assert!(!plugins.get("nvidia.com/gpu").unwrap().allocated.contains("gpu-0"));
}

#[tokio::test]
async fn releasing_for_a_deregistered_resource_is_a_harmless_no_op() {
    let dp = Arc::new(DevicePlugins::new());
    dp.release("never-registered", &["gpu-0".to_string()]);
}
