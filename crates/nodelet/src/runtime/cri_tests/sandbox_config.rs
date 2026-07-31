//! sandbox_config(): the PodSandboxConfig sent to RunPodSandbox. Covers the
//! host-network NamespaceMode::Node wiring (needed for hostNetwork pods to
//! skip CNI) and the log_directory layout that diagnose-coredns-crash.sh's
//! "read the log file directly" approach depends on matching exactly.
use super::*;

fn id(ns: &str, name: &str, uid: &str, host_network: bool) -> PodId {
    PodId { namespace: ns.to_string(), name: name.to_string(), uid: uid.to_string(), host_network, host_users: true }
}

#[test]
fn log_directory_matches_the_ns_name_uid_layout() {
    let cfg = sandbox_config(&id("kube-system", "coredns-abc", "uid-1", false), None, "coredns-abc");
    assert_eq!(cfg.log_directory, "/var/log/pods/kube-system_coredns-abc_uid-1");
}

#[test]
fn non_host_network_pod_sets_hostname_to_pod_name() {
    let cfg = sandbox_config(&id("ns", "myapp", "u", false), None, "myapp");
    assert_eq!(cfg.hostname, "myapp");
}

#[test]
fn non_host_network_pod_honors_an_explicit_resolved_hostname_override() {
    // Round 38: sandbox_config() itself just uses whatever hostname string
    // it's given — resolve_pod_hostname() is what applies spec.hostname/
    // subdomain/setHostnameAsFQDN before this is called.
    let cfg = sandbox_config(&id("ns", "myapp", "u", false), None, "custom-host");
    assert_eq!(cfg.hostname, "custom-host");
}

#[test]
fn host_network_pod_leaves_hostname_empty() {
    // runc rejects setting a hostname when sharing the host UTS namespace.
    let cfg = sandbox_config(&id("ns", "myapp", "u", true), None, "myapp");
    assert_eq!(cfg.hostname, "");
}

#[test]
fn host_network_pod_sets_node_namespace_mode() {
    let cfg = sandbox_config(&id("ns", "myapp", "u", true), None, "myapp");
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
    let cfg = sandbox_config(&id("ns", "myapp", "u", false), None, "myapp");
    assert!(cfg.linux.is_none());
}

#[test]
fn metadata_fields_round_trip_from_pod_id() {
    let cfg = sandbox_config(&id("kube-system", "coredns-abc", "uid-1", false), None, "coredns-abc");
    let meta = cfg.metadata.expect("metadata must be set");
    assert_eq!(meta.name, "coredns-abc");
    assert_eq!(meta.namespace, "kube-system");
    assert_eq!(meta.uid, "uid-1");
    assert_eq!(meta.attempt, 0);
}

#[test]
fn sandbox_labels_are_attached() {
    let cfg = sandbox_config(&id("ns", "n", "u", false), None, "n");
    assert_eq!(cfg.labels.get(POD_NAME_LABEL), Some(&"n".to_string()));
}

// --- userns_mapping (round 25) ---

#[test]
fn a_userns_mapping_forces_a_linux_block_even_without_host_network() {
    let cfg = sandbox_config(&id("ns", "myapp", "u", false), Some((100_000, 65_536)), "myapp");
    assert!(cfg.linux.is_some());
}

#[test]
fn a_userns_mapping_sets_pod_mode_uid_gid_id_mappings() {
    let cfg = sandbox_config(&id("ns", "myapp", "u", false), Some((100_000, 65_536)), "myapp");
    let userns = cfg
        .linux
        .as_ref()
        .and_then(|l| l.security_context.as_ref())
        .and_then(|sc| sc.namespace_options.as_ref())
        .and_then(|no| no.userns_options.as_ref())
        .expect("expected userns_options to be set");
    assert_eq!(userns.mode, NamespaceMode::Pod as i32);
    assert_eq!(userns.uids.len(), 1);
    assert_eq!(userns.uids[0].host_id, 100_000);
    assert_eq!(userns.uids[0].container_id, 0);
    assert_eq!(userns.uids[0].length, 65_536);
    assert_eq!(userns.gids.len(), 1);
    assert_eq!(userns.gids[0].host_id, 100_000);
}

#[test]
fn no_userns_mapping_means_no_userns_options_at_all() {
    let cfg = sandbox_config(&id("ns", "myapp", "u", false), None, "myapp");
    assert!(cfg.linux.is_none());
}
