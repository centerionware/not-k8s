//! `pods_referencing()` — the pure local-cache lookup
//! `on_referenced_object_changed()` uses instead of a real `api.list()`
//! (issue #133): a cluster-wide ConfigMap/Secret `Apply` event used to cost
//! a full namespace pod list on *every* firing, anywhere in the cluster,
//! regardless of whether anything on this node actually referenced the
//! object that changed. These tests exercise the resolution purely in
//! memory, with no live apiserver needed.
use super::*;

fn refs(namespace: &str, name: &str, configmaps: &[&str], secrets: &[&str]) -> PodRefs {
    PodRefs {
        namespace: namespace.to_string(),
        name: name.to_string(),
        configmaps: configmaps.iter().map(|s| s.to_string()).collect(),
        secrets: secrets.iter().map(|s| s.to_string()).collect(),
    }
}

#[test]
fn a_pod_referencing_the_changed_configmap_is_found() {
    let mut cache = HashMap::new();
    cache.insert(pod_key("ns", "web"), refs("ns", "web", &["my-config"], &[]));
    let matches = pods_referencing(&cache, "ns", "my-config", ReferencedKind::ConfigMap);
    assert_eq!(matches, vec!["web".to_string()]);
}

#[test]
fn a_pod_referencing_the_changed_secret_is_found() {
    let mut cache = HashMap::new();
    cache.insert(pod_key("ns", "web"), refs("ns", "web", &[], &["my-secret"]));
    let matches = pods_referencing(&cache, "ns", "my-secret", ReferencedKind::Secret);
    assert_eq!(matches, vec!["web".to_string()]);
}

#[test]
fn configmap_and_secret_kinds_are_not_conflated() {
    // A pod referencing "shared-name" as a ConfigMap must not match a
    // Secret change of the same name, and vice versa.
    let mut cache = HashMap::new();
    cache.insert(pod_key("ns", "web"), refs("ns", "web", &["shared-name"], &[]));
    assert!(pods_referencing(&cache, "ns", "shared-name", ReferencedKind::Secret).is_empty());
    assert_eq!(pods_referencing(&cache, "ns", "shared-name", ReferencedKind::ConfigMap), vec!["web".to_string()]);
}

#[test]
fn a_pod_in_a_different_namespace_is_never_matched() {
    let mut cache = HashMap::new();
    cache.insert(pod_key("other-ns", "web"), refs("other-ns", "web", &["my-config"], &[]));
    assert!(pods_referencing(&cache, "ns", "my-config", ReferencedKind::ConfigMap).is_empty());
}

#[test]
fn an_unrelated_object_anywhere_in_the_cluster_matches_nothing_and_costs_no_lookup() {
    // The whole point of the cache: an Apply event for an object nothing on
    // this node references resolves to an empty match list, purely locally
    // -- this is what replaces the old unconditional api.list() call.
    let mut cache = HashMap::new();
    cache.insert(pod_key("ns", "web"), refs("ns", "web", &["my-config"], &["my-secret"]));
    assert!(pods_referencing(&cache, "ns", "some-other-configmap", ReferencedKind::ConfigMap).is_empty());
    assert!(pods_referencing(&cache, "kube-system", "my-config", ReferencedKind::ConfigMap).is_empty());
}

#[test]
fn multiple_pods_referencing_the_same_object_are_all_found() {
    let mut cache = HashMap::new();
    cache.insert(pod_key("ns", "web-a"), refs("ns", "web-a", &["shared-config"], &[]));
    cache.insert(pod_key("ns", "web-b"), refs("ns", "web-b", &["shared-config"], &[]));
    cache.insert(pod_key("ns", "unrelated"), refs("ns", "unrelated", &["other-config"], &[]));
    let mut matches = pods_referencing(&cache, "ns", "shared-config", ReferencedKind::ConfigMap);
    matches.sort();
    assert_eq!(matches, vec!["web-a".to_string(), "web-b".to_string()]);
}
