//! `NodeUnschedulable` — respect `kubectl cordon`.
//!
//! # Why this exists when TaintToleration would nearly do
//!
//! Cordoning sets `spec.unschedulable`, and the node controller *also* mirrors
//! it as a `node.kubernetes.io/unschedulable:NoSchedule` taint, which
//! `TaintToleration` would catch. Nearly: the mirror is done by a controller,
//! so there is a window after the cordon where the flag is set and the taint
//! is not, and on a cluster where that controller is not running there is no
//! taint at all. Filtering on the flag directly closes both.
//!
//! The toleration is still honoured, because that is how a pod opts into
//! running on a cordoned node — which DaemonSet pods legitimately do.

use crate::cache::{NodeInfo, PodInfo};
use crate::events::{ActionType, ClusterEvent, EventResource};
use crate::framework::status::Status;
use crate::framework::{ClusterEventWithHint, CycleState, FilterPlugin, Plugin};
use k8s_openapi::api::core::v1::Taint;

pub const NAME: &str = "NodeUnschedulable";

/// The taint the node controller mirrors a cordon into, and the one a pod
/// tolerates to opt back in.
const UNSCHEDULABLE_TAINT: &str = "node.kubernetes.io/unschedulable";

pub struct NodeUnschedulable;

impl Plugin for NodeUnschedulable {
    fn name(&self) -> &'static str {
        NAME
    }

    fn events_to_register(&self) -> Vec<ClusterEventWithHint> {
        // An uncordon is projected as a taint-class change (see
        // events::node_action_types, which folds spec.unschedulable into
        // UPDATE_NODE_TAINT precisely so this subscription catches it).
        vec![ClusterEventWithHint::always(ClusterEvent::new(
            EventResource::Node,
            ActionType::ADD | ActionType::UPDATE_NODE_TAINT,
        ))]
    }
}

impl FilterPlugin for NodeUnschedulable {
    fn filter(&self, _state: &CycleState, pod: &PodInfo, node: &NodeInfo) -> Status {
        if !node.unschedulable {
            return Status::success();
        }
        let cordon = Taint {
            key: UNSCHEDULABLE_TAINT.to_string(),
            effect: "NoSchedule".to_string(),
            ..Default::default()
        };
        let tolerated = pod
            .tolerations
            .iter()
            .any(|t| super::taint_toleration::toleration_tolerates_taint(t, &cordon));

        if tolerated {
            Status::success()
        } else {
            // Unresolvable, matching upstream exactly: evicting pods off a
            // cordoned node doesn't uncordon it, so this node is never a
            // preemption candidate for an untolerated cordon.
            Status::unresolvable(NAME, "node(s) were unschedulable")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::framework::plugins::testutil::{node, pod};
    use k8s_openapi::api::core::v1::Toleration;

    #[test]
    fn an_uncordoned_node_admits_everything() {
        assert!(NodeUnschedulable
            .filter(&CycleState::default(), &pod("p"), &node("n"))
            .is_success());
    }

    #[test]
    fn a_cordoned_node_is_rejected() {
        let mut n = node("n");
        n.unschedulable = true;

        let s = NodeUnschedulable.filter(&CycleState::default(), &pod("p"), &n);
        assert!(!s.is_success());
        assert!(s.reasons[0].contains("unschedulable"));
    }

    #[test]
    fn a_cordon_rejection_is_not_resolvable_by_preemption() {
        // Checked directly against upstream's real NodeUnschedulable, which
        // returns UnschedulableAndUnresolvable here: evicting pods off a
        // cordoned node doesn't uncordon it, so preemption can never turn
        // this rejection into a placement — only an actual uncordon can,
        // and that already wakes the pod via this plugin's own
        // events_to_register (see the test below).
        let mut n = node("n");
        n.unschedulable = true;
        let s = NodeUnschedulable.filter(&CycleState::default(), &pod("p"), &n);
        assert!(!s.code.is_resolvable_by_preemption());
    }

    #[test]
    fn a_pod_tolerating_the_cordon_taint_may_still_be_placed() {
        // How DaemonSet pods legitimately run on cordoned nodes.
        let mut n = node("n");
        n.unschedulable = true;
        let mut p = pod("daemon");
        p.tolerations = vec![Toleration {
            key: Some(UNSCHEDULABLE_TAINT.to_string()),
            operator: Some("Exists".to_string()),
            effect: Some("NoSchedule".to_string()),
            ..Default::default()
        }];

        assert!(NodeUnschedulable.filter(&CycleState::default(), &p, &n).is_success());
    }

    #[test]
    fn a_blanket_exists_toleration_also_covers_a_cordon() {
        let mut n = node("n");
        n.unschedulable = true;
        let mut p = pod("superuser");
        p.tolerations = vec![Toleration {
            operator: Some("Exists".to_string()),
            ..Default::default()
        }];

        assert!(NodeUnschedulable.filter(&CycleState::default(), &p, &n).is_success());
    }

    #[test]
    fn an_unrelated_toleration_does_not_cover_a_cordon() {
        let mut n = node("n");
        n.unschedulable = true;
        let mut p = pod("p");
        p.tolerations = vec![Toleration {
            key: Some("dedicated".to_string()),
            operator: Some("Exists".to_string()),
            ..Default::default()
        }];

        assert!(!NodeUnschedulable.filter(&CycleState::default(), &p, &n).is_success());
    }

    #[test]
    fn it_subscribes_to_the_change_an_uncordon_produces() {
        let events = NodeUnschedulable.events_to_register();
        let uncordon =
            ClusterEvent::new(EventResource::Node, ActionType::UPDATE_NODE_TAINT);

        assert!(
            events.iter().any(|e| e.event.matches(&uncordon)),
            "an uncordon must wake pods this plugin rejected"
        );
    }
}
