//! sandbox_labels()/container_labels(): the label set every lookup in this
//! file (find_sandbox, ensure_container's "already have it" check,
//! lookup_pod_by_cid) depends on. A missing/renamed key here silently
//! breaks idempotency checks — this pins the exact label keys down.
use super::*;

fn id(ns: &str, name: &str, uid: &str) -> PodId {
    PodId { namespace: ns.to_string(), name: name.to_string(), uid: uid.to_string(), host_network: false }
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
fn container_labels_include_everything_sandbox_labels_has() {
    let pod_id = id("ns", "n", "u");
    let sandbox = sandbox_labels(&pod_id);
    let container = container_labels(&pod_id, "app", false);
    for (k, v) in &sandbox {
        assert_eq!(container.get(k), Some(v));
    }
}

#[test]
fn container_labels_add_the_container_name_label() {
    let l = container_labels(&id("ns", "n", "u"), "coredns", false);
    assert_eq!(l.get(CTR_NAME_LABEL), Some(&"coredns".to_string()));
}

#[test]
fn container_labels_has_exactly_four_entries() {
    let l = container_labels(&id("ns", "n", "u"), "app", false);
    assert_eq!(l.len(), 4);
}

#[test]
fn init_container_labels_add_a_fifth_init_entry() {
    let l = container_labels(&id("ns", "n", "u"), "setup", true);
    assert_eq!(l.len(), 5);
    assert_eq!(l.get(CTR_INIT_LABEL), Some(&"true".to_string()));
}

#[test]
fn non_init_container_has_no_init_label() {
    let l = container_labels(&id("ns", "n", "u"), "app", false);
    assert_eq!(l.get(CTR_INIT_LABEL), None);
}

#[test]
fn different_pods_produce_different_label_sets() {
    // Sanity check that the labels actually distinguish pods — this is
    // the whole basis for find_sandbox()'s label-selector lookup.
    let a = sandbox_labels(&id("ns", "a", "u1"));
    let b = sandbox_labels(&id("ns", "b", "u2"));
    assert_ne!(a, b);
}
