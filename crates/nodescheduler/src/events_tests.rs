//! Tests for the event diff.
//!
//! The first test in this file is the one that matters most: a node heartbeat
//! must produce no action bits at all. If it ever starts producing some, this
//! component's whole idle-cost claim is gone and nothing else would notice.

use super::*;
use k8s_openapi::api::core::v1::{
    Node, NodeCondition, NodeSpec, NodeStatus, Pod, PodSpec, PodStatus, PodSchedulingGate, Taint,
    Toleration,
};
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::Time;
use std::collections::BTreeMap;

fn ready_condition(status: &str, heartbeat_secs: i64) -> NodeCondition {
    NodeCondition {
        type_: "Ready".to_string(),
        status: status.to_string(),
        last_heartbeat_time: Some(Time(
            k8s_openapi::jiff::Timestamp::from_second(heartbeat_secs).unwrap(),
        )),
        ..Default::default()
    }
}

fn node_with(conditions: Vec<NodeCondition>) -> Node {
    Node {
        status: Some(NodeStatus {
            conditions: Some(conditions),
            allocatable: Some(BTreeMap::from([
                ("cpu".to_string(), Quantity("4".to_string())),
                ("memory".to_string(), Quantity("8Gi".to_string())),
            ])),
            ..Default::default()
        }),
        ..Default::default()
    }
}

#[test]
fn a_node_heartbeat_wakes_absolutely_nothing() {
    // Exactly what kubelet sends every node-monitor-period: the same
    // conditions, with a newer lastHeartbeatTime. On a 200-node cluster this
    // is 20 events a second at complete idle, so it has to cost nothing.
    let old = node_with(vec![ready_condition("True", 1_000)]);
    let new = node_with(vec![ready_condition("True", 1_010)]);

    assert_eq!(
        node_action_types(&old, &new),
        ActionType::NONE,
        "a heartbeat-only Node update must produce no action bits — see events.rs's header"
    );
}

#[test]
fn a_node_going_notready_is_a_condition_change() {
    let old = node_with(vec![ready_condition("True", 1_000)]);
    let new = node_with(vec![ready_condition("False", 1_010)]);

    assert!(node_action_types(&old, &new).contains(ActionType::UPDATE_NODE_CONDITION));
}

#[test]
fn reordering_conditions_is_not_a_change() {
    let a = ready_condition("True", 1_000);
    let b = NodeCondition {
        type_: "MemoryPressure".to_string(),
        status: "False".to_string(),
        ..Default::default()
    };
    let old = node_with(vec![a.clone(), b.clone()]);
    let new = node_with(vec![b, a]);

    assert_eq!(node_action_types(&old, &new), ActionType::NONE);
}

#[test]
fn a_new_taint_sets_only_the_taint_bit() {
    let mut old = node_with(vec![ready_condition("True", 1_000)]);
    old.spec = Some(NodeSpec::default());
    let mut new = old.clone();
    new.spec = Some(NodeSpec {
        taints: Some(vec![Taint {
            key: "dedicated".to_string(),
            value: Some("gpu".to_string()),
            effect: "NoSchedule".to_string(),
            ..Default::default()
        }]),
        ..Default::default()
    });

    let action = node_action_types(&old, &new);
    assert!(action.contains(ActionType::UPDATE_NODE_TAINT));
    assert!(!action.contains(ActionType::UPDATE_NODE_CONDITION));
    assert!(!action.contains(ActionType::UPDATE_NODE_ALLOCATABLE));
}

#[test]
fn cordoning_a_node_counts_as_a_taint_change() {
    // NodeUnschedulable filters on spec.unschedulable directly, so a cordon
    // has to wake pods even on a cluster where the node controller isn't
    // mirroring it into a real taint.
    let mut old = node_with(vec![]);
    old.spec = Some(NodeSpec::default());
    let mut new = old.clone();
    new.spec = Some(NodeSpec {
        unschedulable: Some(true),
        ..Default::default()
    });

    assert!(node_action_types(&old, &new).contains(ActionType::UPDATE_NODE_TAINT));
}

#[test]
fn changed_allocatable_sets_only_the_allocatable_bit() {
    let old = node_with(vec![ready_condition("True", 1_000)]);
    let mut new = old.clone();
    new.status.as_mut().unwrap().allocatable = Some(BTreeMap::from([
        ("cpu".to_string(), Quantity("8".to_string())),
        ("memory".to_string(), Quantity("8Gi".to_string())),
    ]));

    let action = node_action_types(&old, &new);
    assert!(action.contains(ActionType::UPDATE_NODE_ALLOCATABLE));
    assert!(!action.contains(ActionType::UPDATE_NODE_CONDITION));
}

// ── Pods ────────────────────────────────────────────────────────────────

fn gated_pod(gates: &[&str]) -> Pod {
    Pod {
        spec: Some(PodSpec {
            scheduling_gates: Some(
                gates
                    .iter()
                    .map(|g| PodSchedulingGate { name: g.to_string() })
                    .collect(),
            ),
            ..Default::default()
        }),
        ..Default::default()
    }
}

#[test]
fn clearing_the_last_scheduling_gate_is_its_own_bit() {
    let old = gated_pod(&["example.com/hold"]);
    let new = gated_pod(&[]);

    assert!(pod_action_types(&old, &new)
        .contains(ActionType::UPDATE_POD_SCHEDULING_GATES_ELIMINATED));
}

#[test]
fn removing_one_of_two_gates_does_not_eliminate_them() {
    // Still gated, still unschedulable — waking every gated pod here would be
    // pure waste on a cluster using multiple gates.
    let old = gated_pod(&["a", "b"]);
    let new = gated_pod(&["b"]);

    assert!(!pod_action_types(&old, &new)
        .contains(ActionType::UPDATE_POD_SCHEDULING_GATES_ELIMINATED));
}

#[test]
fn a_pod_label_change_sets_only_the_label_bit() {
    let old = Pod::default();
    let mut new = Pod::default();
    new.metadata.labels = Some(BTreeMap::from([("app".to_string(), "web".to_string())]));

    let action = pod_action_types(&old, &new);
    assert!(action.contains(ActionType::UPDATE_POD_LABEL));
    assert!(!action.contains(ActionType::UPDATE_POD_TOLERATION));
}

#[test]
fn a_new_toleration_sets_the_toleration_bit() {
    let old = Pod {
        spec: Some(PodSpec::default()),
        ..Default::default()
    };
    let new = Pod {
        spec: Some(PodSpec {
            tolerations: Some(vec![Toleration {
                key: Some("dedicated".to_string()),
                operator: Some("Exists".to_string()),
                ..Default::default()
            }]),
            ..Default::default()
        }),
        ..Default::default()
    };

    assert!(pod_action_types(&old, &new).contains(ActionType::UPDATE_POD_TOLERATION));
}

#[test]
fn a_pure_status_churn_update_produces_nothing() {
    // Container statuses and probe results move constantly and mean nothing
    // to placement.
    let old = Pod {
        spec: Some(PodSpec::default()),
        status: Some(PodStatus {
            phase: Some("Pending".to_string()),
            ..Default::default()
        }),
        ..Default::default()
    };
    let mut new = old.clone();
    new.status.as_mut().unwrap().phase = Some("Running".to_string());

    assert_eq!(pod_action_types(&old, &new), ActionType::NONE);
}

// ── Matching ────────────────────────────────────────────────────────────

#[test]
fn a_plugin_registered_for_taints_is_not_woken_by_a_label_change() {
    let registered = ClusterEvent::new(EventResource::Node, ActionType::UPDATE_NODE_TAINT);
    let happened = ClusterEvent::new(EventResource::Node, ActionType::UPDATE_NODE_LABEL);

    assert!(!registered.matches(&happened));
}

#[test]
fn a_plugin_registered_for_taints_is_woken_by_a_taint_change() {
    let registered = ClusterEvent::new(EventResource::Node, ActionType::UPDATE_NODE_TAINT);
    let happened = ClusterEvent::new(
        EventResource::Node,
        ActionType::UPDATE_NODE_TAINT | ActionType::UPDATE_NODE_LABEL,
    );

    assert!(registered.matches(&happened));
}

#[test]
fn the_timeout_safety_net_matches_everything() {
    for resource in [
        EventResource::Node,
        EventResource::Pod,
        EventResource::PersistentVolume,
        EventResource::ResourceClaim,
    ] {
        let registered = ClusterEvent::new(resource, ActionType::ADD);
        assert!(
            registered.matches(&UNSCHEDULABLE_TIMEOUT),
            "the 5-minute net has to reach a plugin registered for {resource:?}"
        );
    }
}

#[test]
fn a_generic_pod_subscription_covers_both_pod_flavours() {
    let registered = ClusterEvent::new(EventResource::Pod, ActionType::DELETE);
    for resource in [EventResource::AssignedPod, EventResource::UnschedulablePod] {
        assert!(registered.matches(&ClusterEvent::new(resource, ActionType::DELETE)));
    }
}

#[test]
fn an_empty_action_matches_nothing() {
    let registered = ClusterEvent::new(EventResource::Node, ActionType::ALL);
    assert!(!registered.matches(&ClusterEvent::new(EventResource::Node, ActionType::NONE)));
}
