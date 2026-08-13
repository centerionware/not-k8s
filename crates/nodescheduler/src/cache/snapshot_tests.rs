//! Tests for the cache and the incremental snapshot.
//!
//! The first test is the one that matters: the snapshot must reuse the `Arc`
//! of every node that did not change. If it ever starts deep-copying the
//! cluster per cycle, nothing fails — the scheduler just gets quadratically
//! slower as the cluster grows, which is invisible on a test rig and fatal in
//! production.

use super::*;
use crate::cache::pod::{PodInfo, Resources};
use k8s_openapi::api::core::v1::Node;
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
