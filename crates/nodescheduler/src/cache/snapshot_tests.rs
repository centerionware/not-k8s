//! Tests for the cache and the incremental snapshot.
//!
//! The first test is the one that matters: the snapshot must reuse the `Arc`
//! of every node that did not change. If it ever starts deep-copying the
//! cluster per cycle, nothing fails — the scheduler just gets quadratically
//! slower as the cluster grows, which is invisible on a test rig and fatal in
//! production.

use super::*;
use crate::cache::pod::{PodInfo, Resources};
use k8s_openapi::api::core::v1::{ContainerImage, Node, NodeStatus};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;

fn node(name: &str) -> Node {
    Node {
        metadata: ObjectMeta { name: Some(name.to_string()), ..Default::default() },
        ..Default::default()
    }
}

fn node_in_zone(name: &str, zone: &str) -> Node {
    Node {
        metadata: ObjectMeta {
            name: Some(name.to_string()),
            labels: Some(BTreeMap::from([(
                "topology.kubernetes.io/zone".to_string(),
                zone.to_string(),
            )])),
            ..Default::default()
        },
        ..Default::default()
    }
}

fn node_with_images(name: &str, images: &[&str]) -> Node {
    Node {
        metadata: ObjectMeta { name: Some(name.to_string()), ..Default::default() },
        status: Some(NodeStatus {
            images: Some(vec![ContainerImage {
                names: Some(images.iter().map(|image| image.to_string()).collect()),
                size_bytes: Some(100),
            }]),
            ..Default::default()
        }),
        ..Default::default()
    }
}

fn pod_on(uid: &str, node_name: &str, milli_cpu: i64) -> Arc<PodInfo> {
    Arc::new(PodInfo {
        uid: uid.to_string(),
        name: uid.to_string(),
        node_name: Some(node_name.to_string()),
        requests: Resources { milli_cpu, ..Default::default() },
        ..Default::default()
    })
}

#[test]
fn refreshing_a_snapshot_copies_only_the_nodes_that_changed() {
    // THE scalability property. Three changed nodes must cost three clones
    // whatever the cluster size.
    let mut cache = Cache::new();
    for i in 0..50 {
        cache.upsert_node(&node(&format!("n{i}")));
    }
    let mut snap = cache.snapshot();
    let before: HashMap<String, Arc<NodeInfo>> = snap
        .nodes()
        .iter()
        .map(|n| (n.name.clone(), n.clone()))
        .collect();

    cache.add_pod(pod_on("p", "n7", 100));
    cache.update_snapshot(&mut snap);

    let mut rebuilt = 0;
    for n in snap.nodes() {
        let old = &before[&n.name];
        if !Arc::ptr_eq(old, n) {
            rebuilt += 1;
            assert_eq!(n.name, "n7", "only the touched node may be rebuilt");
        }
    }
    assert_eq!(rebuilt, 1, "49 untouched nodes must be reused by pointer, not cloned");
}

#[test]
fn a_snapshot_taken_twice_with_no_changes_does_nothing() {
    let mut cache = Cache::new();
    cache.upsert_node(&node("a"));
    let mut snap = cache.snapshot();
    let first: Vec<Arc<NodeInfo>> = snap.nodes().to_vec();

    cache.update_snapshot(&mut snap);

    for (a, b) in first.iter().zip(snap.nodes()) {
        assert!(Arc::ptr_eq(a, b));
    }
}

#[test]
fn image_spread_is_maintained_on_node_add_update_and_delete() {
    let mut cache = Cache::new();
    cache.upsert_node(&node_with_images("a", &["app:v1", "sidecar:v1"]));
    cache.upsert_node(&node_with_images("b", &["app:v1"]));
    let mut snap = cache.snapshot();
    assert_eq!(snap.nodes_with_image("app:v1"), 2);
    assert_eq!(snap.image_node_counts.get("app:v1"), Some(&2));

    cache.upsert_node(&node_with_images("b", &["replacement:v1"]));
    cache.update_snapshot(&mut snap);
    assert_eq!(snap.image_node_counts.get("app:v1"), Some(&1));
    assert_eq!(snap.image_node_counts.get("replacement:v1"), Some(&1));

    cache.remove_node("a");
    cache.update_snapshot(&mut snap);
    assert!(!snap.image_node_counts.contains_key("app:v1"));
    assert!(!snap.image_node_counts.contains_key("sidecar:v1"));
}

#[test]
fn a_snapshot_sees_a_pod_committed_after_it_was_taken() {
    let mut cache = Cache::new();
    cache.upsert_node(&node("a"));
    let mut snap = cache.snapshot();
    assert_eq!(snap.node("a").unwrap().requested.milli_cpu, 0);

    cache.add_pod(pod_on("p", "a", 500));
    cache.update_snapshot(&mut snap);

    assert_eq!(snap.node("a").unwrap().requested.milli_cpu, 500);
}

#[test]
fn a_pod_only_change_does_not_disturb_the_zone_round_robin_order() {
    // The fast path (`node_positions`) must land the patched node in exactly
    // the slot a full `zone_round_robin` rebuild would have put it — a stale
    // slot silently un-interleaves the zones, which only PodTopologySpread's
    // scoring would ever notice.
    let mut cache = Cache::new();
    cache.upsert_node(&node_in_zone("a1", "zone-a"));
    cache.upsert_node(&node_in_zone("a2", "zone-a"));
    cache.upsert_node(&node_in_zone("b1", "zone-b"));
    let mut snap = cache.snapshot();
    let order_before: Vec<String> = snap.nodes().iter().map(|n| n.name.clone()).collect();

    // A pod landing on an existing node changes nothing about membership or
    // zones, so this must take the in-place patch path, not a reorder.
    cache.add_pod(pod_on("p", "a2", 100));
    cache.update_snapshot(&mut snap);

    let order_after: Vec<String> = snap.nodes().iter().map(|n| n.name.clone()).collect();
    assert_eq!(order_before, order_after, "a pod-only change must not reshuffle node order");
    assert_eq!(snap.node("a2").unwrap().requested.milli_cpu, 100);

    // And the order must still be a real zone round robin — the fast path
    // must not have simply frozen a stale order incidentally.
    let fresh = cache.snapshot();
    let fresh_order: Vec<String> = fresh.nodes().iter().map(|n| n.name.clone()).collect();
    assert_eq!(order_after, fresh_order);
}

#[test]
fn a_node_changing_zone_does_trigger_a_reorder() {
    let mut cache = Cache::new();
    cache.upsert_node(&node_in_zone("a1", "zone-a"));
    cache.upsert_node(&node_in_zone("b1", "zone-b"));
    let mut snap = cache.snapshot();

    // Move a1 into zone-b — a real membership-shape change from the ordering
    // walk's point of view, even though the node itself isn't new.
    cache.upsert_node(&node_in_zone("a1", "zone-b"));
    cache.update_snapshot(&mut snap);

    let fresh = cache.snapshot();
    let fresh_order: Vec<String> = fresh.nodes().iter().map(|n| n.name.clone()).collect();
    let order: Vec<String> = snap.nodes().iter().map(|n| n.name.clone()).collect();
    assert_eq!(order, fresh_order, "a zone change must be reflected, not skipped by the fast path");
}

#[test]
fn a_removed_node_disappears_from_a_refreshed_snapshot() {
    // A deletion leaves nothing behind in the MRU walk, so the walk alone
    // would keep serving a node that no longer exists — and the scheduler
    // would bind pods to it.
    let mut cache = Cache::new();
    cache.upsert_node(&node("a"));
    cache.upsert_node(&node("b"));
    let mut snap = cache.snapshot();
    assert_eq!(snap.num_nodes(), 2);

    cache.remove_node("a");
    cache.update_snapshot(&mut snap);

    assert_eq!(snap.num_nodes(), 1);
    assert!(snap.node("a").is_none());
    assert!(snap.node("b").is_some());
}

#[test]
fn removing_a_pod_releases_it_without_the_caller_naming_the_node() {
    // Deletions routinely arrive carrying only a uid.
    let mut cache = Cache::new();
    cache.upsert_node(&node("a"));
    cache.add_pod(pod_on("p", "a", 500));
    assert_eq!(cache.pod_node("p"), Some("a"));

    cache.remove_pod("p");

    let snap = cache.snapshot();
    assert_eq!(snap.node("a").unwrap().requested.milli_cpu, 0);
    assert_eq!(cache.pod_node("p"), None);
}

#[test]
fn a_pod_that_reappears_on_another_node_is_released_from_the_first() {
    // Force-deleted and recreated under the same uid. Without the release,
    // its resources stay committed to a node that is not running it — a leak
    // that only ever makes the cluster look fuller.
    let mut cache = Cache::new();
    cache.upsert_node(&node("a"));
    cache.upsert_node(&node("b"));

    cache.add_pod(pod_on("p", "a", 500));
    cache.add_pod(pod_on("p", "b", 500));

    let snap = cache.snapshot();
    assert_eq!(snap.node("a").unwrap().requested.milli_cpu, 0);
    assert_eq!(snap.node("b").unwrap().requested.milli_cpu, 500);
}

#[test]
fn a_pod_for_an_unknown_node_is_dropped_rather_than_buffered() {
    // Transient at startup when the pod watch lists before the node watch.
    // The node watch's own list carries these pods again, so buffering would
    // only risk double-counting.
    let mut cache = Cache::new();
    cache.add_pod(pod_on("p", "not-yet-seen", 500));
    assert_eq!(cache.num_nodes(), 0);
    assert_eq!(cache.pod_node("p"), None);
}

#[test]
fn node_order_alternates_between_zones() {
    // Otherwise a cluster whose node names sort by rack packs one rack first.
    let mut cache = Cache::new();
    cache.upsert_node(&node_in_zone("a1", "east"));
    cache.upsert_node(&node_in_zone("a2", "east"));
    cache.upsert_node(&node_in_zone("b1", "west"));
    cache.upsert_node(&node_in_zone("b2", "west"));

    let snap = cache.snapshot();
    let zones: Vec<&str> = snap
        .nodes()
        .iter()
        .map(|n| n.labels["topology.kubernetes.io/zone"].as_str())
        .collect();

    assert_eq!(zones, vec!["east", "west", "east", "west"]);
}

#[test]
fn unlabelled_nodes_still_get_a_complete_stable_order() {
    // The common case on bare metal — they must not be dropped.
    let mut cache = Cache::new();
    for n in ["c", "a", "b"] {
        cache.upsert_node(&node(n));
    }
    let snap = cache.snapshot();
    let names: Vec<&str> = snap.nodes().iter().map(|n| n.name.as_str()).collect();
    assert_eq!(names, vec!["a", "b", "c"]);
}

#[test]
fn zones_of_uneven_size_still_place_every_node_exactly_once() {
    let mut cache = Cache::new();
    cache.upsert_node(&node_in_zone("a1", "east"));
    cache.upsert_node(&node_in_zone("b1", "west"));
    cache.upsert_node(&node_in_zone("b2", "west"));
    cache.upsert_node(&node_in_zone("b3", "west"));

    let snap = cache.snapshot();
    let mut names: Vec<&str> = snap.nodes().iter().map(|n| n.name.as_str()).collect();
    assert_eq!(names.len(), 4);
    names.sort_unstable();
    assert_eq!(names, vec!["a1", "b1", "b2", "b3"]);
}

#[test]
fn a_node_update_does_not_lose_the_pods_committed_to_it() {
    // Node updates arrive constantly; losing the totals on each one would
    // make every node look empty whenever anything about it changed.
    let mut cache = Cache::new();
    cache.upsert_node(&node("a"));
    cache.add_pod(pod_on("p", "a", 500));

    cache.upsert_node(&node("a"));

    let snap = cache.snapshot();
    assert_eq!(snap.node("a").unwrap().requested.milli_cpu, 500);
    assert_eq!(snap.node("a").unwrap().pods.len(), 1);
}

#[test]
fn the_affinity_subsets_track_which_nodes_actually_have_such_pods() {
    use k8s_openapi::api::core::v1::{Affinity, PodAffinityTerm, PodAntiAffinity};

    let mut cache = Cache::new();
    cache.upsert_node(&node("a"));
    cache.upsert_node(&node("b"));
    cache.add_pod(pod_on("plain", "a", 0));
    cache.add_pod(Arc::new(PodInfo {
        uid: "spread".into(),
        node_name: Some("b".into()),
        affinity: Some(Box::new(Affinity {
            pod_anti_affinity: Some(PodAntiAffinity {
                required_during_scheduling_ignored_during_execution: Some(vec![
                    PodAffinityTerm {
                        topology_key: "kubernetes.io/hostname".to_string(),
                        ..Default::default()
                    },
                ]),
                ..Default::default()
            }),
            ..Default::default()
        })),
        ..Default::default()
    }));

    let snap = cache.snapshot();
    let names: Vec<&str> = snap
        .nodes_with_pods_with_required_anti_affinity
        .iter()
        .map(|n| n.name.as_str())
        .collect();
    assert_eq!(names, vec!["b"]);
}

// ── Metadata: namespaces and workload selectors ─────────────────────────

fn label_map(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
}

fn selector_for(pairs: &[(&str, &str)]) -> k8s_openapi::apimachinery::pkg::apis::meta::v1::LabelSelector {
    k8s_openapi::apimachinery::pkg::apis::meta::v1::LabelSelector {
        match_labels: Some(label_map(pairs)),
        match_expressions: None,
    }
}

#[test]
fn a_namespace_change_alone_still_refreshes_the_snapshot() {
    // The incremental walk is driven by node generations, so a change that
    // touches no node finds nothing to copy. Returning early there would
    // leave the snapshot holding namespace labels from startup forever — an
    // affinity rule that stops applying, with nothing in any log to say why.
    let mut cache = Cache::new();
    cache.upsert_node(&node("n1"));
    let mut snap = Snapshot::default();
    cache.update_snapshot(&mut snap);
    assert!(snap.namespaces.is_empty());

    cache.upsert_namespace("prod", label_map(&[("env", "prod")]));
    cache.update_snapshot(&mut snap);
    assert_eq!(snap.namespaces.get("prod"), Some(&label_map(&[("env", "prod")])));

    cache.remove_namespace("prod");
    cache.update_snapshot(&mut snap);
    assert!(snap.namespaces.is_empty(), "a deleted namespace must leave the snapshot");
}

#[test]
fn a_replicaset_and_a_service_of_the_same_name_both_survive() {
    // Keyed by kind as well as name. Collapsing them drops one selector, and
    // the pod then spreads against half of what it should.
    let mut cache = Cache::new();
    cache.upsert_workload(
        "rs/default/web".to_string(),
        WorkloadSelector { namespace: "default".to_string(), selector: selector_for(&[("app", "web")]) },
    );
    cache.upsert_workload(
        "svc/default/web".to_string(),
        WorkloadSelector {
            namespace: "default".to_string(),
            selector: selector_for(&[("tier", "front")]),
        },
    );
    let mut snap = Snapshot::default();
    cache.update_snapshot(&mut snap);
    assert_eq!(snap.workload_selectors.len(), 2);

    let matches = snap.matching_workload_selectors("default", &label_map(&[("app", "web")]));
    assert_eq!(matches.len(), 1, "only the selector that actually matches applies");

    cache.remove_workload("svc/default/web");
    cache.update_snapshot(&mut snap);
    assert_eq!(snap.workload_selectors.len(), 1);
}

#[test]
fn workload_selectors_do_not_cross_namespaces() {
    let mut cache = Cache::new();
    cache.upsert_workload(
        "rs/other/web".to_string(),
        WorkloadSelector { namespace: "other".to_string(), selector: selector_for(&[("app", "web")]) },
    );
    let mut snap = Snapshot::default();
    cache.update_snapshot(&mut snap);
    assert!(snap.matching_workload_selectors("default", &label_map(&[("app", "web")])).is_empty());
    assert_eq!(snap.matching_workload_selectors("other", &label_map(&[("app", "web")])).len(), 1);
}

// ── Phase 4: storage objects ──────────────────────────────────────────────

#[test]
fn a_pv_upsert_is_visible_in_the_next_snapshot_and_gone_after_removal() {
    let mut cache = Cache::new();
    cache.upsert_pv("pv-1".to_string(), crate::cache::PvInfo { name: "pv-1".to_string(), ..Default::default() });
    let mut snap = Snapshot::default();
    cache.update_snapshot(&mut snap);
    assert!(snap.pv("pv-1").is_some());

    cache.remove_pv("pv-1");
    cache.update_snapshot(&mut snap);
    assert!(snap.pv("pv-1").is_none());
}

#[test]
fn a_pvc_and_storage_class_upsert_round_trip_through_the_snapshot() {
    let mut cache = Cache::new();
    cache.upsert_pvc(
        "ns/claim".to_string(),
        crate::cache::PvcInfo { namespace: "ns".to_string(), name: "claim".to_string(), ..Default::default() },
    );
    cache.upsert_storage_class(
        "standard".to_string(),
        crate::cache::StorageClassInfo { name: "standard".to_string(), ..Default::default() },
    );
    let mut snap = Snapshot::default();
    cache.update_snapshot(&mut snap);

    assert!(snap.pvc("ns", "claim").is_some());
    assert!(snap.storage_class("standard").is_some());
}

#[test]
fn a_storage_only_mutation_still_refreshes_the_snapshot_with_no_node_changes() {
    // The same trap namespaces/workload selectors already guard against: a
    // mutation that touches no node must not be dropped by the "nothing
    // changed" early return, or storage data goes stale forever.
    let mut cache = Cache::new();
    let mut snap = cache.snapshot();
    cache.upsert_storage_class(
        "standard".to_string(),
        crate::cache::StorageClassInfo { name: "standard".to_string(), ..Default::default() },
    );
    cache.update_snapshot(&mut snap);
    assert!(snap.storage_class("standard").is_some());
}

#[test]
fn pods_using_a_pvc_are_found_across_every_node() {
    let mut cache = Cache::new();
    cache.upsert_node(&node("n1"));
    cache.upsert_node(&node("n2"));
    let mut a = (*pod_on("a", "n1", 100)).clone();
    a.namespace = "ns".to_string();
    a.pvc_names = vec!["data".to_string()];
    let mut b = (*pod_on("b", "n2", 100)).clone();
    b.namespace = "ns".to_string();
    b.pvc_names = vec!["data".to_string()];
    let mut unrelated = (*pod_on("c", "n1", 100)).clone();
    unrelated.namespace = "ns".to_string();
    cache.add_pod(Arc::new(a));
    cache.add_pod(Arc::new(b));
    cache.add_pod(Arc::new(unrelated));

    let snap = cache.snapshot();
    let users: Vec<&str> = snap.pods_using_pvc("ns", "data").map(|p| p.uid.as_str()).collect();
    assert_eq!(users.len(), 2, "both nodes' pods must be found, not just the local one");
    assert!(users.contains(&"a"));
    assert!(users.contains(&"b"));
}
