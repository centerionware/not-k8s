//! node_labels(): the label set every Node object gets, including
//! user-supplied labels from NODELET_NODE_LABELS.
use super::*;
use std::collections::BTreeMap;
use std::time::Duration;

fn cfg(labels: BTreeMap<String, String>) -> Config {
    Config {
        node_name: "debian".to_string(),
        runtime: crate::config::RuntimeKind::Mock,
        cri_endpoint: String::new(),
        heartbeat: Duration::from_secs(10),
        status_interval: Duration::from_secs(60),
        cpu_cores: 1,
        memory_bytes: 1,
        max_pods: 1,
        labels,
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
    }
}

#[test]
fn includes_hostname_from_node_name() {
    let l = node_labels(&cfg(BTreeMap::new()));
    assert_eq!(l.get("kubernetes.io/hostname"), Some(&"debian".to_string()));
}

#[test]
fn includes_os_and_arch() {
    let l = node_labels(&cfg(BTreeMap::new()));
    assert_eq!(l.get("kubernetes.io/os"), Some(&std::env::consts::OS.to_string()));
    assert_eq!(l.get("kubernetes.io/arch"), Some(&std::env::consts::ARCH.to_string()));
}

#[test]
fn marks_itself_as_nodelet_managed() {
    let l = node_labels(&cfg(BTreeMap::new()));
    assert_eq!(l.get("nodelet.dev/managed"), Some(&"true".to_string()));
    assert_eq!(l.get("node.kubernetes.io/instance-type"), Some(&"nodelet".to_string()));
}

#[test]
fn user_supplied_labels_are_merged_in() {
    let mut extra = BTreeMap::new();
    extra.insert("region".to_string(), "edge-1".to_string());
    let l = node_labels(&cfg(extra));
    assert_eq!(l.get("region"), Some(&"edge-1".to_string()));
    // Built-in labels are still present alongside the custom one.
    assert!(l.contains_key("nodelet.dev/managed"));
}

#[test]
fn user_supplied_label_can_override_a_builtin_key() {
    // Whether this is desirable is arguable, but it must be deterministic
    // (last-write-wins from cfg.labels), not silently dropped either way.
    let mut extra = BTreeMap::new();
    extra.insert("nodelet.dev/managed".to_string(), "false".to_string());
    let l = node_labels(&cfg(extra));
    assert_eq!(l.get("nodelet.dev/managed"), Some(&"false".to_string()));
}
