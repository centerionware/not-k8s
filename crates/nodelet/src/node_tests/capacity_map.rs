//! capacity_map(): what nodelet reports for Node.status.capacity/allocatable.
//! A wrong unit here would misreport scheduling capacity to the whole cluster.
use super::*;
use std::time::Duration;

fn cfg(cpu: u64, mem: u64, pods: u64) -> Config {
    Config {
        node_name: "test-node".to_string(),
        runtime: crate::config::RuntimeKind::Mock,
        cri_endpoint: String::new(),
        heartbeat: Duration::from_secs(10),
        status_interval: Duration::from_secs(60),
        cpu_cores: cpu,
        memory_bytes: mem,
        max_pods: pods,
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
    }
}

#[test]
fn reports_cpu_as_a_plain_core_count_not_millicores() {
    // Quantity("8") means 8 whole cores; Quantity("8000m") would also mean
    // 8 cores but in a totally different unit — if this ever accidentally
    // appended "m", every capacity number would be misread as 1000x too small.
    let m = capacity_map(&cfg(8, 1, 1));
    assert_eq!(m.get("cpu").unwrap().0, "8");
}

#[test]
fn reports_memory_in_raw_bytes() {
    let m = capacity_map(&cfg(1, 2984013824, 1));
    assert_eq!(m.get("memory").unwrap().0, "2984013824");
}

#[test]
fn reports_max_pods() {
    let m = capacity_map(&cfg(1, 1, 110));
    assert_eq!(m.get("pods").unwrap().0, "110");
}

#[test]
fn has_exactly_the_three_expected_keys() {
    let m = capacity_map(&cfg(1, 1, 1));
    assert_eq!(m.len(), 3);
    assert!(m.contains_key("cpu"));
    assert!(m.contains_key("memory"));
    assert!(m.contains_key("pods"));
}

#[test]
fn zero_values_are_reported_as_zero_not_omitted() {
    let m = capacity_map(&cfg(0, 0, 0));
    assert_eq!(m.get("cpu").unwrap().0, "0");
    assert_eq!(m.get("memory").unwrap().0, "0");
    assert_eq!(m.get("pods").unwrap().0, "0");
}
