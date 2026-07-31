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
        userns_base_uid: 100_000,
        userns_length: 65_536,
        userns_max_pods: 1024,
    }
}

#[test]
fn no_extra_capacity_leaves_only_the_standard_four_resources() {
    let status = build_status(&cfg(), true, &BTreeMap::new(), Vec::new(), &[]);
    let cap = status.capacity.unwrap();
    assert_eq!(cap.len(), 4);
}

#[test]
fn device_plugin_resources_are_added_to_capacity() {
    let mut extra = BTreeMap::new();
    extra.insert("nvidia.com/gpu".to_string(), 2u64);
    let status = build_status(&cfg(), true, &extra, Vec::new(), &[]);
    let cap = status.capacity.unwrap();
    assert_eq!(cap.get("nvidia.com/gpu").unwrap().0, "2");
    assert_eq!(cap.len(), 5);
}

#[test]
fn device_plugin_resources_also_appear_in_allocatable() {
    let mut extra = BTreeMap::new();
    extra.insert("nvidia.com/gpu".to_string(), 2u64);
    let status = build_status(&cfg(), true, &extra, Vec::new(), &[]);
    let alloc = status.allocatable.unwrap();
    assert_eq!(alloc.get("nvidia.com/gpu").unwrap().0, "2");
}

#[test]
fn multiple_extended_resources_all_appear() {
    let mut extra = BTreeMap::new();
    extra.insert("nvidia.com/gpu".to_string(), 1u64);
    extra.insert("example.com/fpga".to_string(), 3u64);
    let status = build_status(&cfg(), true, &extra, Vec::new(), &[]);
    let cap = status.capacity.unwrap();
    assert_eq!(cap.get("nvidia.com/gpu").unwrap().0, "1");
    assert_eq!(cap.get("example.com/fpga").unwrap().0, "3");
}

#[test]
fn zero_healthy_devices_still_reports_the_resource_as_zero() {
    let mut extra = BTreeMap::new();
    extra.insert("nvidia.com/gpu".to_string(), 0u64);
    let status = build_status(&cfg(), true, &extra, Vec::new(), &[]);
    assert_eq!(status.capacity.unwrap().get("nvidia.com/gpu").unwrap().0, "0");
}

// --- images (round 33) ---

fn img(names: &[&str], size_bytes: u64) -> crate::runtime::NodeImage {
    crate::runtime::NodeImage { names: names.iter().map(|s| s.to_string()).collect(), size_bytes }
}

#[test]
fn no_images_at_all_produces_an_empty_but_present_list() {
    let status = build_status(&cfg(), true, &BTreeMap::new(), Vec::new(), &[]);
    assert!(status.images.unwrap().is_empty());
}

#[test]
fn images_are_reported_largest_first() {
    let images = vec![img(&["small:latest"], 100), img(&["big:latest"], 900), img(&["medium:latest"], 500)];
    let status = build_status(&cfg(), true, &BTreeMap::new(), images, &[]);
    let reported = status.images.unwrap();
    assert_eq!(reported.len(), 3);
    assert_eq!(reported[0].names.as_ref().unwrap()[0], "big:latest");
    assert_eq!(reported[1].names.as_ref().unwrap()[0], "medium:latest");
    assert_eq!(reported[2].names.as_ref().unwrap()[0], "small:latest");
}

#[test]
fn image_names_and_size_round_trip() {
    let images = vec![img(&["repo:v1", "repo@sha256:abc"], 12345)];
    let status = build_status(&cfg(), true, &BTreeMap::new(), images, &[]);
    let reported = status.images.unwrap();
    assert_eq!(reported[0].names.as_ref().unwrap(), &vec!["repo:v1".to_string(), "repo@sha256:abc".to_string()]);
    assert_eq!(reported[0].size_bytes, Some(12345));
}

#[test]
fn more_than_fifty_images_are_capped_to_the_fifty_largest() {
    let images: Vec<_> = (0..75).map(|i| img(&["x"], i)).collect();
    let status = build_status(&cfg(), true, &BTreeMap::new(), images, &[]);
    let reported = status.images.unwrap();
    assert_eq!(reported.len(), 50);
    // The 50 largest of 0..75 are 25..75, so the smallest kept is 74.
    assert_eq!(reported[0].size_bytes, Some(74));
    assert_eq!(reported[49].size_bytes, Some(25));
}

// --- volumesInUse/volumesAttached (round 34) ---

#[test]
fn no_mounted_csi_volumes_produces_empty_but_present_lists() {
    let status = build_status(&cfg(), true, &BTreeMap::new(), Vec::new(), &[]);
    assert!(status.volumes_in_use.unwrap().is_empty());
    assert!(status.volumes_attached.unwrap().is_empty());
}

#[test]
fn a_mounted_csi_volume_appears_in_both_lists_with_the_kubelet_naming_scheme() {
    let mounted = vec![("csi.example.com".to_string(), "vol-1".to_string())];
    let status = build_status(&cfg(), true, &BTreeMap::new(), Vec::new(), &mounted);
    let expected = "kubernetes.io/csi/csi.example.com^vol-1".to_string();
    assert_eq!(status.volumes_in_use.unwrap(), vec![expected.clone()]);
    let attached = status.volumes_attached.unwrap();
    assert_eq!(attached.len(), 1);
    assert_eq!(attached[0].name, expected);
    assert_eq!(attached[0].device_path, "");
}

#[test]
fn multiple_mounted_volumes_all_appear() {
    let mounted =
        vec![("driver-a".to_string(), "vol-a".to_string()), ("driver-b".to_string(), "vol-b".to_string())];
    let status = build_status(&cfg(), true, &BTreeMap::new(), Vec::new(), &mounted);
    assert_eq!(status.volumes_in_use.unwrap().len(), 2);
    assert_eq!(status.volumes_attached.unwrap().len(), 2);
}
