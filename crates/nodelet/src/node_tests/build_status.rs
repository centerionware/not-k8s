//! build_status()'s extra_capacity merge — device plugin resources
//! (nvidia.com/gpu and friends) get folded into Node.status.capacity/
//! allocatable alongside cpu/memory/pods.
use super::*;
use std::time::Duration;

fn cfg() -> Config {
    Config {
        node_name: "test-node".to_string(),
        runtime: crate::config::RuntimeKind::Mock,
        cri_endpoint: String::new(),
        heartbeat: Duration::from_secs(10),
        status_interval: Duration::from_secs(60),
        cpu_cores: 4,
        memory_bytes: 8_000_000_000,
        max_pods: 110,
        labels: Default::default(),
        service_proxy: false,
        ip_family: crate::config::IpFamily::V4,
        lb_method: crate::config::LbMethod::Random,
        memory_pressure_threshold_bytes: 100 * 1024 * 1024,
        disk_path: "/tmp".to_string(),
        disk_pressure_percent: 10,
        pid_pressure_percent: 10,
        container_log_max_size_bytes: 10 * 1024 * 1024,
        container_log_max_files: 5,
        log_rotate_interval: Duration::from_secs(10),
        static_pod_path: None,
        static_pod_sync_interval: Duration::from_secs(20),
        server_enabled: false,
        server_port: 10250,
        server_cert_dir: "/tmp".to_string(),
        gc_interval: Duration::from_secs(300),
        cluster_dns: Vec::new(),
        cluster_domain: "cluster.local".to_string(),
        eviction_check_interval: Duration::from_secs(10),
        shutdown_grace_period_seconds: 0,
        shutdown_grace_period_critical_seconds: 0,
        system_reserved_cpu_millicores: 0,
        system_reserved_memory_bytes: 0,
        kube_reserved_cpu_millicores: 0,
        kube_reserved_memory_bytes: 0,
        cgroup_fs_root: "/sys/fs/cgroup".to_string(),
        csi_drivers: Default::default(),
        plugin_registry_path: "/tmp".to_string(),
        plugin_registry_sync_interval: Duration::from_secs(10),
        cpu_manager_static: false,
        topology_manager_policy: "none".to_string(),
        memory_manager_static: false,
    }
}

#[test]
fn no_extra_capacity_leaves_only_the_standard_three_resources() {
    let status = build_status(&cfg(), true, &BTreeMap::new());
    let cap = status.capacity.unwrap();
    assert_eq!(cap.len(), 3);
}

#[test]
fn device_plugin_resources_are_added_to_capacity() {
    let mut extra = BTreeMap::new();
    extra.insert("nvidia.com/gpu".to_string(), 2u64);
    let status = build_status(&cfg(), true, &extra);
    let cap = status.capacity.unwrap();
    assert_eq!(cap.get("nvidia.com/gpu").unwrap().0, "2");
    assert_eq!(cap.len(), 4);
}

#[test]
fn device_plugin_resources_also_appear_in_allocatable() {
    let mut extra = BTreeMap::new();
    extra.insert("nvidia.com/gpu".to_string(), 2u64);
    let status = build_status(&cfg(), true, &extra);
    let alloc = status.allocatable.unwrap();
    assert_eq!(alloc.get("nvidia.com/gpu").unwrap().0, "2");
}

#[test]
fn multiple_extended_resources_all_appear() {
    let mut extra = BTreeMap::new();
    extra.insert("nvidia.com/gpu".to_string(), 1u64);
    extra.insert("example.com/fpga".to_string(), 3u64);
    let status = build_status(&cfg(), true, &extra);
    let cap = status.capacity.unwrap();
    assert_eq!(cap.get("nvidia.com/gpu").unwrap().0, "1");
    assert_eq!(cap.get("example.com/fpga").unwrap().0, "3");
}

#[test]
fn zero_healthy_devices_still_reports_the_resource_as_zero() {
    let mut extra = BTreeMap::new();
    extra.insert("nvidia.com/gpu".to_string(), 0u64);
    let status = build_status(&cfg(), true, &extra);
    assert_eq!(status.capacity.unwrap().get("nvidia.com/gpu").unwrap().0, "0");
}
