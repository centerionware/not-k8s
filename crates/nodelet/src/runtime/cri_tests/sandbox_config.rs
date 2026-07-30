//! sandbox_config(): the PodSandboxConfig sent to RunPodSandbox. Covers the
//! host-network NamespaceMode::Node wiring (needed for hostNetwork pods to
//! skip CNI) and the log_directory layout that diagnose-coredns-crash.sh's
//! "read the log file directly" approach depends on matching exactly.
use super::*;

fn id(ns: &str, name: &str, uid: &str, host_network: bool) -> PodId {
    PodId { namespace: ns.to_string(), name: name.to_string(), uid: uid.to_string(), host_network }
}

#[test]
fn log_directory_matches_the_ns_name_uid_layout() {
    let cfg = sandbox_config(&id("kube-system", "coredns-abc", "uid-1", false));
    assert_eq!(cfg.log_directory, "/var/log/pods/kube-system_coredns-abc_uid-1");
}

#[test]
fn non_host_network_pod_sets_hostname_to_pod_name() {
    let cfg = sandbox_config(&id("ns", "myapp", "u", false));
    assert_eq!(cfg.hostname, "myapp");
}

#[test]
fn host_network_pod_leaves_hostname_empty() {
    // runc rejects setting a hostname when sharing the host UTS namespace.
    let cfg = sandbox_config(&id("ns", "myapp", "u", true));
    assert_eq!(cfg.hostname, "");
}

#[test]
fn host_network_pod_sets_node_namespace_mode() {
    let cfg = sandbox_config(&id("ns", "myapp", "u", true));
    let ns_mode = cfg
        .linux
        .as_ref()
        .and_then(|l| l.security_context.as_ref())
        .and_then(|sc| sc.namespace_options.as_ref())
        .map(|no| no.network);
    assert_eq!(ns_mode, Some(NamespaceMode::Node as i32));
}

#[test]
fn non_host_network_pod_has_no_linux_namespace_override() {
    let cfg = sandbox_config(&id("ns", "myapp", "u", false));
    assert!(cfg.linux.is_none());
}

#[test]
fn metadata_fields_round_trip_from_pod_id() {
    let cfg = sandbox_config(&id("kube-system", "coredns-abc", "uid-1", false));
    let meta = cfg.metadata.expect("metadata must be set");
    assert_eq!(meta.name, "coredns-abc");
    assert_eq!(meta.namespace, "kube-system");
    assert_eq!(meta.uid, "uid-1");
    assert_eq!(meta.attempt, 0);
}

#[test]
fn sandbox_labels_are_attached() {
    let cfg = sandbox_config(&id("ns", "n", "u", false));
    assert_eq!(cfg.labels.get(POD_NAME_LABEL), Some(&"n".to_string()));
}
