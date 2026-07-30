//! build_node(): the Node object nodelet applies. The regression this
//! guards is specific and already bit real deployments — see the comment
//! on build_node() itself: setting spec.providerID used to be believed
//! (wrongly) to be what triggered the cloudprovider-uninitialized taint.
//! It's never set here; taint removal is handled separately by
//! clear_cloudprovider_taint()/taints_without() (see taint_filter.rs).
use super::*;
use std::time::Duration;

fn cfg() -> Config {
    Config {
        node_name: "debian".to_string(),
        runtime: crate::config::RuntimeKind::Cri,
        cri_endpoint: String::new(),
        heartbeat: Duration::from_secs(10),
        status_interval: Duration::from_secs(60),
        cpu_cores: 8,
        memory_bytes: 2984013824,
        max_pods: 110,
        labels: Default::default(),
        service_proxy: true,
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
        gc_interval: Duration::from_secs(300),
        cluster_dns: Vec::new(),
        cluster_domain: "cluster.local".to_string(),
        eviction_check_interval: Duration::from_secs(10),
    }
}

#[test]
fn name_matches_config_node_name() {
    let n = build_node(&cfg());
    assert_eq!(n.metadata.name.as_deref(), Some("debian"));
}

#[test]
fn provider_id_is_never_set() {
    // Setting one tells the node-lifecycle-controller a cloud-controller-
    // manager will show up to initialize the node — there isn't one here.
    let n = build_node(&cfg());
    assert!(n.spec.as_ref().unwrap().provider_id.is_none());
}

#[test]
fn status_is_not_set_by_build_node() {
    // Status is applied separately via push_status()/patch_status — a
    // Patch::Apply that also carried status would fight with that
    // subresource-scoped patch over field ownership.
    let n = build_node(&cfg());
    assert!(n.status.is_none());
}

#[test]
fn labels_are_attached() {
    let n = build_node(&cfg());
    let labels = n.metadata.labels.unwrap();
    assert_eq!(labels.get("kubernetes.io/hostname"), Some(&"debian".to_string()));
}
