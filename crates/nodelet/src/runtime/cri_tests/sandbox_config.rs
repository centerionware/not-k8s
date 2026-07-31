//! sandbox_config(): the PodSandboxConfig sent to RunPodSandbox. Covers the
//! host-network NamespaceMode::Node wiring (needed for hostNetwork pods to
//! skip CNI), the log_directory layout that diagnose-coredns-crash.sh's
//! "read the log file directly" approach depends on matching exactly, and
//! (round 40) hostPID/hostIPC/shareProcessNamespace.
use super::*;

fn id(ns: &str, name: &str, uid: &str, host_network: bool) -> PodId {
    PodId {
        namespace: ns.to_string(),
        name: name.to_string(),
        uid: uid.to_string(),
        host_network,
        host_users: true,
        host_pid: false,
        host_ipc: false,
        share_process_namespace: false,
    }
}

fn ns_options(cfg: &PodSandboxConfig) -> NamespaceOption {
    cfg.linux
        .as_ref()
        .and_then(|l| l.security_context.as_ref())
        .and_then(|sc| sc.namespace_options.clone())
        .expect("expected namespace_options to be set")
}

#[test]
fn log_directory_matches_the_ns_name_uid_layout() {
    let cfg = sandbox_config(&id("kube-system", "coredns-abc", "uid-1", false), None, "coredns-abc", &HashMap::new());
    assert_eq!(cfg.log_directory, "/var/log/pods/kube-system_coredns-abc_uid-1");
}

#[test]
fn non_host_network_pod_sets_hostname_to_pod_name() {
    let cfg = sandbox_config(&id("ns", "myapp", "u", false), None, "myapp", &HashMap::new());
    assert_eq!(cfg.hostname, "myapp");
}

#[test]
fn non_host_network_pod_honors_an_explicit_resolved_hostname_override() {
    // Round 38: sandbox_config() itself just uses whatever hostname string
    // it's given — resolve_pod_hostname() is what applies spec.hostname/
    // subdomain/setHostnameAsFQDN before this is called.
    let cfg = sandbox_config(&id("ns", "myapp", "u", false), None, "custom-host", &HashMap::new());
    assert_eq!(cfg.hostname, "custom-host");
}

#[test]
fn host_network_pod_leaves_hostname_empty() {
    // runc rejects setting a hostname when sharing the host UTS namespace.
    let cfg = sandbox_config(&id("ns", "myapp", "u", true), None, "myapp", &HashMap::new());
    assert_eq!(cfg.hostname, "");
}

#[test]
fn host_network_pod_sets_node_namespace_mode() {
    let cfg = sandbox_config(&id("ns", "myapp", "u", true), None, "myapp", &HashMap::new());
    assert_eq!(ns_options(&cfg).network, NamespaceMode::Node as i32);
}

#[test]
fn non_host_network_pod_sets_pod_namespace_mode_for_network() {
    let cfg = sandbox_config(&id("ns", "myapp", "u", false), None, "myapp", &HashMap::new());
    assert_eq!(ns_options(&cfg).network, NamespaceMode::Pod as i32);
}

#[test]
fn metadata_fields_round_trip_from_pod_id() {
    let cfg = sandbox_config(&id("kube-system", "coredns-abc", "uid-1", false), None, "coredns-abc", &HashMap::new());
    let meta = cfg.metadata.expect("metadata must be set");
    assert_eq!(meta.name, "coredns-abc");
    assert_eq!(meta.namespace, "kube-system");
    assert_eq!(meta.uid, "uid-1");
    assert_eq!(meta.attempt, 0);
}

#[test]
fn sandbox_labels_are_attached() {
    let cfg = sandbox_config(&id("ns", "n", "u", false), None, "n", &HashMap::new());
    assert_eq!(cfg.labels.get(POD_NAME_LABEL), Some(&"n".to_string()));
}

// --- userns_mapping (round 25) ---

#[test]
fn a_userns_mapping_sets_pod_mode_uid_gid_id_mappings() {
    let cfg = sandbox_config(&id("ns", "myapp", "u", false), Some((100_000, 65_536)), "myapp", &HashMap::new());
    let userns = ns_options(&cfg).userns_options.expect("expected userns_options to be set");
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
    let cfg = sandbox_config(&id("ns", "myapp", "u", false), None, "myapp", &HashMap::new());
    assert!(ns_options(&cfg).userns_options.is_none());
}

// --- hostPID/hostIPC/shareProcessNamespace (round 40) ---

#[test]
fn default_pod_gets_container_scoped_pid_and_pod_scoped_ipc() {
    // The correctness fix this round is about: CRI's own proto default for
    // an unset `pid` is POD (every container shares one), the opposite of
    // real Kubernetes' actual default. This must always be set explicitly.
    let cfg = sandbox_config(&id("ns", "myapp", "u", false), None, "myapp", &HashMap::new());
    let no = ns_options(&cfg);
    assert_eq!(no.pid, NamespaceMode::Container as i32);
    assert_eq!(no.ipc, NamespaceMode::Pod as i32);
}

#[test]
fn host_pid_sets_node_pid_namespace() {
    let mut pod_id = id("ns", "myapp", "u", false);
    pod_id.host_pid = true;
    let cfg = sandbox_config(&pod_id, None, "myapp", &HashMap::new());
    assert_eq!(ns_options(&cfg).pid, NamespaceMode::Node as i32);
}

#[test]
fn host_ipc_sets_node_ipc_namespace() {
    let mut pod_id = id("ns", "myapp", "u", false);
    pod_id.host_ipc = true;
    let cfg = sandbox_config(&pod_id, None, "myapp", &HashMap::new());
    assert_eq!(ns_options(&cfg).ipc, NamespaceMode::Node as i32);
}

#[test]
fn share_process_namespace_sets_pod_scoped_pid_namespace() {
    let mut pod_id = id("ns", "myapp", "u", false);
    pod_id.share_process_namespace = true;
    let cfg = sandbox_config(&pod_id, None, "myapp", &HashMap::new());
    assert_eq!(ns_options(&cfg).pid, NamespaceMode::Pod as i32);
}

#[test]
fn host_pid_wins_over_share_process_namespace() {
    let mut pod_id = id("ns", "myapp", "u", false);
    pod_id.host_pid = true;
    pod_id.share_process_namespace = true;
    let cfg = sandbox_config(&pod_id, None, "myapp", &HashMap::new());
    assert_eq!(ns_options(&cfg).pid, NamespaceMode::Node as i32);
}

// --- sysctls (round 41) ---

#[test]
fn sysctls_are_passed_through_to_the_linux_config() {
    let mut sysctls = HashMap::new();
    sysctls.insert("net.core.somaxconn".to_string(), "1024".to_string());
    let cfg = sandbox_config(&id("ns", "myapp", "u", false), None, "myapp", &sysctls);
    assert_eq!(cfg.linux.unwrap().sysctls.get("net.core.somaxconn"), Some(&"1024".to_string()));
}

#[test]
fn no_sysctls_means_an_empty_map_not_a_missing_field() {
    let cfg = sandbox_config(&id("ns", "myapp", "u", false), None, "myapp", &HashMap::new());
    assert!(cfg.linux.unwrap().sysctls.is_empty());
}
