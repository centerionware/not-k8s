//! Tests for the node projection and its incremental totals.
//!
//! The add/remove pairing is the thing under test. Everything downstream
//! trusts `requested` without recomputing it, so a leak here is invisible
//! until a node quietly stops accepting pods it has room for.

use super::*;
use crate::cache::pod::PodInfo;
use k8s_openapi::api::core::v1::{Node, NodeCondition, NodeSpec, NodeStatus, Taint};
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;

fn pod_needing(uid: &str, milli_cpu: i64, memory: i64) -> Arc<PodInfo> {
    Arc::new(PodInfo {
        uid: uid.to_string(),
        name: uid.to_string(),
        requests: Resources { milli_cpu, memory, ..Default::default() },
        ..Default::default()
    })
}

#[test]
fn adding_and_removing_a_pod_returns_the_node_to_where_it_started() {
    let mut n = NodeInfo::default();
    let before = n.requested.clone();

    n.add_pod(pod_needing("a", 500, 1024), 1);
    assert_eq!(n.requested.milli_cpu, 500);

    n.remove_pod("a", 2);
    assert_eq!(n.requested, before, "the totals must not leak across a pod's lifetime");
    assert!(n.pods.is_empty());
}

#[test]
fn re_adding_the_same_pod_does_not_double_count_it() {
    // The real sequence that produces this: our own cycle assumes the pod,
    // then the informer delivers the bound object. Double-counting would make
    // every node this scheduler placed onto look twice as full as it is.
    let mut n = NodeInfo::default();
    n.add_pod(pod_needing("a", 500, 1024), 1);
    n.add_pod(pod_needing("a", 500, 1024), 2);

    assert_eq!(n.pods.len(), 1);
    assert_eq!(n.requested.milli_cpu, 500);
}

#[test]
fn every_mutation_bumps_the_generation() {
    // The snapshot copies a node only when its generation moved, so a
    // mutation that forgets to bump is a node the scheduler never sees change.
    let mut n = NodeInfo::default();
    n.add_pod(pod_needing("a", 1, 1), 7);
    assert_eq!(n.generation, 7);
    n.remove_pod("a", 9);
    assert_eq!(n.generation, 9);
}

#[test]
fn removing_a_pod_that_was_never_here_changes_nothing() {
    let mut n = NodeInfo::default();
    n.add_pod(pod_needing("a", 500, 0), 1);
    assert!(n.remove_pod("ghost", 2).is_none());
    assert_eq!(n.requested.milli_cpu, 500);
    assert_eq!(n.pods.len(), 1);
}

#[test]
fn non_zero_totals_track_the_substitution_per_pod_not_per_node() {
    // A pod requesting nothing still contributes 100m/200Mi to the scoring
    // total. That cannot be recovered from `requested` after summing, which
    // is why the two totals are kept side by side.
    let mut n = NodeInfo::default();
    n.add_pod(Arc::new(PodInfo { uid: "empty".into(), ..Default::default() }), 1);

    assert_eq!(n.requested.milli_cpu, 0);
    assert_eq!(n.non_zero_requested.milli_cpu, crate::cache::pod::DEFAULT_MILLI_CPU_REQUEST);
    assert_eq!(n.non_zero_requested.memory, crate::cache::pod::DEFAULT_MEMORY_REQUEST);
}

#[test]
fn free_capacity_never_goes_negative() {
    let mut n = NodeInfo {
        allocatable: Resources { milli_cpu: 1000, ..Default::default() },
        ..Default::default()
    };
    n.add_pod(pod_needing("greedy", 4000, 0), 1);
    assert_eq!(n.free().milli_cpu, 0);
}

#[test]
fn a_released_host_port_can_be_claimed_again() {
    let mut n = NodeInfo::default();
    let pod = Arc::new(PodInfo {
        uid: "web".into(),
        host_ports: vec![HostPort { protocol: "TCP".into(), ip: String::new(), port: 80 }],
        ..Default::default()
    });
    let want = HostPort { protocol: "TCP".into(), ip: "10.0.0.5".into(), port: 80 };

    n.add_pod(pod, 1);
    assert!(n.port_conflicts(&want));

    n.remove_pod("web", 2);
    assert!(!n.port_conflicts(&want), "the port must be released with the pod");
}

#[test]
fn only_pods_with_anti_affinity_land_in_the_prefiltered_subset() {
    use k8s_openapi::api::core::v1::{Affinity, PodAffinityTerm, PodAntiAffinity};

    let mut n = NodeInfo::default();
    n.add_pod(pod_needing("plain", 0, 0), 1);
    n.add_pod(
        Arc::new(PodInfo {
            uid: "spread".into(),
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
        }),
        2,
    );

    assert_eq!(n.pods.len(), 2);
    assert_eq!(n.pods_with_required_anti_affinity.len(), 1);
    assert_eq!(n.pods_with_required_anti_affinity[0].uid, "spread");
}

#[test]
fn projecting_a_node_keeps_the_pods_already_committed_to_it() {
    // Node updates arrive constantly (labels, conditions, allocatable). If
    // re-projecting reset the running totals, every node would appear empty
    // the moment anything about it changed.
    let mut n = NodeInfo::default();
    n.add_pod(pod_needing("a", 500, 0), 1);

    let node = Node {
        metadata: k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta {
            name: Some("worker-1".to_string()),
            ..Default::default()
        },
        status: Some(NodeStatus {
            allocatable: Some(BTreeMap::from([
                ("cpu".to_string(), Quantity("4".to_string())),
                ("pods".to_string(), Quantity("110".to_string())),
            ])),
            ..Default::default()
        }),
        ..Default::default()
    };
    n.update_from_node(&node, 2);

    assert_eq!(n.name, "worker-1");
    assert_eq!(n.allocatable.milli_cpu, 4000);
    assert_eq!(n.allocatable_pods, 110);
    assert_eq!(n.requested.milli_cpu, 500, "committed pods survive a node update");
    assert_eq!(n.pods.len(), 1);
}

#[test]
fn conditions_are_projected_without_their_heartbeat_timestamps() {
    // Dropping the timestamps is the point of the projection — they are the
    // bulk of node update traffic and mean nothing to placement.
    let node = Node {
        status: Some(NodeStatus {
            conditions: Some(vec![NodeCondition {
                type_: "Ready".to_string(),
                status: "True".to_string(),
                last_heartbeat_time: Some(
                    k8s_openapi::apimachinery::pkg::apis::meta::v1::Time(
                        k8s_openapi::jiff::Timestamp::from_second(1_000).unwrap(),
                    ),
                ),
                ..Default::default()
            }]),
            ..Default::default()
        }),
        ..Default::default()
    };
    let mut n = NodeInfo::default();
    n.update_from_node(&node, 1);

    assert_eq!(n.conditions.get("Ready").map(String::as_str), Some("True"));
    assert!(n.is_ready());
}

#[test]
fn a_cordoned_node_is_projected_as_unschedulable() {
    let node = Node {
        spec: Some(NodeSpec { unschedulable: Some(true), ..Default::default() }),
        ..Default::default()
    };
    let mut n = NodeInfo::default();
    n.update_from_node(&node, 1);
    assert!(n.unschedulable);
}

#[test]
fn taints_are_projected_verbatim() {
    let node = Node {
        spec: Some(NodeSpec {
            taints: Some(vec![Taint {
                key: "dedicated".to_string(),
                value: Some("gpu".to_string()),
                effect: "NoSchedule".to_string(),
                ..Default::default()
            }]),
            ..Default::default()
        }),
        ..Default::default()
    };
    let mut n = NodeInfo::default();
    n.update_from_node(&node, 1);

    assert_eq!(n.taints.len(), 1);
    assert_eq!(n.taints[0].effect, "NoSchedule");
}
