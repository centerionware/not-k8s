//! sandbox_labels()/container_labels(): the label set every lookup in this
//! file (find_sandbox, ensure_container's "already have it" check,
//! lookup_pod_by_cid) depends on. A missing/renamed key here silently
//! breaks idempotency checks — this pins the exact label keys down.
use super::*;

fn id(ns: &str, name: &str, uid: &str) -> PodId {
    PodId {
        namespace: ns.to_string(),
        name: name.to_string(),
        uid: uid.to_string(),
        host_network: false,
        host_users: true,
        host_pid: false,
        host_ipc: false,
        share_process_namespace: false,
        service_account_name: "default".to_string(),
    }
}

#[test]
fn sandbox_labels_include_uid_name_and_namespace() {
    let l = sandbox_labels(&id("kube-system", "coredns-abc", "uid-1"));
    assert_eq!(l.get(POD_UID_LABEL), Some(&"uid-1".to_string()));
    assert_eq!(l.get(POD_NAME_LABEL), Some(&"coredns-abc".to_string()));
    assert_eq!(l.get(POD_NS_LABEL), Some(&"kube-system".to_string()));
}

#[test]
fn sandbox_labels_has_exactly_three_entries() {
    // A stray extra label wouldn't break anything, but a missing one would
    // — pin the count so a typo'd/duplicated key is caught too.
    let l = sandbox_labels(&id("ns", "n", "u"));
    assert_eq!(l.len(), 3);
}

#[test]
fn host_network_sandbox_labels_identify_the_node_network_namespace() {
    let mut pod_id = id("ns", "n", "u");
    pod_id.host_network = true;
    let labels = sandbox_labels(&pod_id);
    assert_eq!(labels.get(HOST_NETWORK_LABEL), Some(&"true".to_string()));
    assert_eq!(labels.len(), 4);
}

#[test]
fn container_labels_include_everything_sandbox_labels_has() {
    let pod_id = id("ns", "n", "u");
    let sandbox = sandbox_labels(&pod_id);
    let container = container_labels(&pod_id, "app", ContainerKind::App);
    for (k, v) in &sandbox {
        assert_eq!(container.get(k), Some(v));
    }
}

#[test]
fn container_labels_add_the_container_name_label() {
    let l = container_labels(&id("ns", "n", "u"), "coredns", ContainerKind::App);
    assert_eq!(l.get(CTR_NAME_LABEL), Some(&"coredns".to_string()));
}

#[test]
fn container_labels_has_exactly_four_entries() {
    let l = container_labels(&id("ns", "n", "u"), "app", ContainerKind::App);
    assert_eq!(l.len(), 4);
}

#[test]
fn init_container_labels_add_a_fifth_init_entry() {
    let l = container_labels(&id("ns", "n", "u"), "setup", ContainerKind::Init);
    assert_eq!(l.len(), 5);
    assert_eq!(l.get(CTR_INIT_LABEL), Some(&"true".to_string()));
}

#[test]
fn non_init_container_has_no_init_label() {
    let l = container_labels(&id("ns", "n", "u"), "app", ContainerKind::App);
    assert_eq!(l.get(CTR_INIT_LABEL), None);
}

#[test]
fn ephemeral_container_labels_add_a_fifth_ephemeral_entry() {
    let l = container_labels(&id("ns", "n", "u"), "debugger", ContainerKind::Ephemeral);
    assert_eq!(l.len(), 5);
    assert_eq!(l.get(CTR_EPHEMERAL_LABEL), Some(&"true".to_string()));
}

#[test]
fn ephemeral_container_has_no_init_label_and_init_container_has_no_ephemeral_label() {
    let ephemeral = container_labels(&id("ns", "n", "u"), "debugger", ContainerKind::Ephemeral);
    assert_eq!(ephemeral.get(CTR_INIT_LABEL), None);
    let init = container_labels(&id("ns", "n", "u"), "setup", ContainerKind::Init);
    assert_eq!(init.get(CTR_EPHEMERAL_LABEL), None);
}

#[test]
fn different_pods_produce_different_label_sets() {
    // Sanity check that the labels actually distinguish pods — this is
    // the whole basis for find_sandbox()'s label-selector lookup.
    let a = sandbox_labels(&id("ns", "a", "u1"));
    let b = sandbox_labels(&id("ns", "b", "u2"));
    assert_ne!(a, b);
}

#[test]
fn ephemeral_to_container_carries_the_debug_relevant_fields_and_drops_ports() {
    let ec = EphemeralContainer {
        name: "debugger".to_string(),
        image: Some("busybox:latest".to_string()),
        command: Some(vec!["sh".to_string()]),
        tty: Some(true),
        stdin: Some(true),
        target_container_name: Some("app".to_string()),
        ports: Some(vec![Default::default()]),
        ..Default::default()
    };
    let c = ephemeral_to_container(&ec);
    assert_eq!(c.name, "debugger");
    assert_eq!(c.image.as_deref(), Some("busybox:latest"));
    assert_eq!(c.command, Some(vec!["sh".to_string()]));
    assert_eq!(c.tty, Some(true));
    assert_eq!(c.stdin, Some(true));
    assert_eq!(c.ports, None); // real kubelet ignores ports on ephemeral containers too
}
