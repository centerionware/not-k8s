//! `TaintToleration` — filter on `NoSchedule`/`NoExecute`, score on
//! `PreferNoSchedule`. Score weight **3**, the highest in the default profile.
//!
//! # Why the weight is 3 and why that is easy to get wrong
//!
//! The published v1.33 reference table shows every score plugin at weight 1
//! except PodTopologySpread. That table is stale. `default_plugins.go` in
//! `release-1.33` gives TaintToleration 3, which makes it dominate the other
//! scorers — a `PreferNoSchedule` taint is a strong hint, not a tiebreak.
//! A wrong weight here fails nothing and simply places pods differently from
//! upstream forever, which is the exact divergence this project exists not to
//! have.
//!
//! # The two effects are handled in different places
//!
//! `NoExecute` is filtered here for *scheduling*, but evicting pods already
//! running on a node that acquires a `NoExecute` taint is the node-lifecycle
//! controller's job, not the scheduler's. This plugin only ever answers "may
//! this pod be placed here", never "must that pod leave".

use crate::cache::{NodeInfo, PodInfo};
use crate::events::{ActionType, ClusterEvent, EventResource};
use crate::framework::status::Status;
use crate::framework::{
    ClusterEventWithHint, CycleState, FilterPlugin, Plugin, PreScorePlugin, ScorePlugin,
};
use k8s_openapi::api::core::v1::{Taint, Toleration};

pub const NAME: &str = "TaintToleration";

/// Whether one toleration covers one taint.
///
/// Shared with `NodeUnschedulable`, which asks the same question about the
/// built-in `node.kubernetes.io/unschedulable` taint.
///
/// The empty cases are the subtle ones and all three are real API semantics,
/// not leniency: an empty `key` with `operator: Exists` tolerates *every*
/// taint, an empty `effect` tolerates every effect of the matching key, and
/// `operator` itself defaults to `Equal` when unset. `tolerationSeconds` is
/// deliberately ignored — it bounds how long an already-running pod survives
/// a `NoExecute` taint, which is eviction's concern, not placement's.
pub fn toleration_tolerates_taint(tol: &Toleration, taint: &Taint) -> bool {
    if let Some(effect) = tol.effect.as_deref() {
        if !effect.is_empty() && effect != taint.effect {
            return false;
        }
    }
    let operator = tol.operator.as_deref().unwrap_or("Equal");
    match operator {
        "Exists" => match tol.key.as_deref() {
            None | Some("") => true,
            Some(k) => k == taint.key,
        },
        // "Equal", and anything unrecognised — the apiserver validates the
        // enum, so an unknown operator is a malformed object, and refusing to
        // tolerate is the safe reading of one.
        _ => {
            tol.key.as_deref().unwrap_or("") == taint.key
                && tol.value.as_deref().unwrap_or("")
                    == taint.value.as_deref().unwrap_or("")
        }
    }
}

/// The first taint of any listed effect that nothing in `tolerations` covers.
pub fn find_untolerated_taint<'a>(
    taints: &'a [Taint],
    tolerations: &[Toleration],
    effects: &[&str],
) -> Option<&'a Taint> {
    taints.iter().find(|taint| {
        effects.contains(&taint.effect.as_str())
            && !tolerations.iter().any(|t| toleration_tolerates_taint(t, taint))
    })
}

pub struct TaintToleration;

impl Plugin for TaintToleration {
    fn name(&self) -> &'static str {
        NAME
    }

    fn events_to_register(&self) -> Vec<ClusterEventWithHint> {
        // A pod rejected here becomes schedulable when a taint is removed, or
        // when a node that tolerates it appears. Nothing else can help it —
        // notably not a node's allocatable or conditions changing, which is
        // why this is two specific subscriptions rather than `Node/UPDATE`.
        vec![
            ClusterEventWithHint::always(ClusterEvent::new(
                EventResource::Node,
                ActionType::ADD | ActionType::UPDATE_NODE_TAINT,
            )),
            // The pod's own tolerations can be edited to cover the taint.
            ClusterEventWithHint::always(ClusterEvent::new(
                EventResource::Pod,
                ActionType::UPDATE_POD_TOLERATION,
            )),
        ]
    }
}

impl FilterPlugin for TaintToleration {
    fn filter(&self, _state: &CycleState, pod: &PodInfo, node: &NodeInfo) -> Status {
        match find_untolerated_taint(
            &node.taints,
            &pod.tolerations,
            &["NoSchedule", "NoExecute"],
        ) {
            None => Status::success(),
            Some(taint) => Status::unschedulable(
                NAME,
                format!(
                    "node(s) had untolerated taint {{{}: {}}}",
                    taint.key,
                    taint.value.as_deref().unwrap_or("")
                ),
            ),
        }
    }
}

/// Raw score: how many `PreferNoSchedule` taints this pod does *not* tolerate.
/// More is worse, which is why normalization runs reversed.
fn count_intolerable_prefer_no_schedule(pod: &PodInfo, node: &NodeInfo) -> i64 {
    node.taints
        .iter()
        .filter(|t| t.effect == "PreferNoSchedule")
        .filter(|t| !pod.tolerations.iter().any(|tol| toleration_tolerates_taint(tol, t)))
        .count() as i64
}

impl PreScorePlugin for TaintToleration {
    fn pre_score(&self, state: &mut CycleState, pod: &PodInfo, _nodes: &[&NodeInfo]) -> Status {
        // A pod tolerating nothing still needs scoring — untolerated taints
        // are exactly what it is penalised for. The only genuinely skippable
        // case is a cluster with no PreferNoSchedule taints at all, which the
        // scorer already answers as a uniform zero, so there is nothing to
        // gain from detecting it here.
        state.write(NAME, ());
        let _ = pod;
        Status::success()
    }
}

impl ScorePlugin for TaintToleration {
    fn score(&self, _state: &CycleState, pod: &PodInfo, node: &NodeInfo) -> Result<i64, Status> {
        Ok(count_intolerable_prefer_no_schedule(pod, node))
    }

    fn normalize(&self, _state: &CycleState, _pod: &PodInfo, scores: &mut [i64]) -> Status {
        super::default_normalize_score(true, scores);
        Status::success()
    }

    fn weight(&self) -> i64 {
        3
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::framework::plugins::testutil::{node, pod};
    use crate::framework::status::Code;

    fn taint(key: &str, value: &str, effect: &str) -> Taint {
        Taint {
            key: key.to_string(),
            value: Some(value.to_string()),
            effect: effect.to_string(),
            ..Default::default()
        }
    }

    fn tol(key: Option<&str>, op: &str, value: Option<&str>, effect: Option<&str>) -> Toleration {
        Toleration {
            key: key.map(String::from),
            operator: Some(op.to_string()),
            value: value.map(String::from),
            effect: effect.map(String::from),
            ..Default::default()
        }
    }

    #[test]
    fn an_untolerated_noschedule_taint_rejects_the_node() {
        let mut n = node("tainted");
        n.taints = vec![taint("dedicated", "gpu", "NoSchedule")];

        let s = TaintToleration.filter(&CycleState::default(), &pod("p"), &n);
        assert_eq!(s.code, Code::Unschedulable);
        assert!(s.reasons[0].contains("untolerated taint"));
    }

    #[test]
    fn a_rejection_here_is_resolvable_by_preemption_never_unresolvable() {
        // Not obvious, and load-bearing: a taint can be removed, so a node
        // rejected for one stays a preemption candidate. Marking it
        // unresolvable would silently disable preemption on tainted nodes.
        let mut n = node("tainted");
        n.taints = vec![taint("dedicated", "gpu", "NoSchedule")];
        let s = TaintToleration.filter(&CycleState::default(), &pod("p"), &n);
        assert!(s.code.is_resolvable_by_preemption());
    }

    #[test]
    fn an_exact_equal_toleration_admits_the_pod() {
        let mut n = node("tainted");
        n.taints = vec![taint("dedicated", "gpu", "NoSchedule")];
        let mut p = pod("p");
        p.tolerations = vec![tol(Some("dedicated"), "Equal", Some("gpu"), Some("NoSchedule"))];

        assert!(TaintToleration.filter(&CycleState::default(), &p, &n).is_success());
    }

    #[test]
    fn an_equal_toleration_with_the_wrong_value_does_not_admit_the_pod() {
        let mut n = node("tainted");
        n.taints = vec![taint("dedicated", "gpu", "NoSchedule")];
        let mut p = pod("p");
        p.tolerations = vec![tol(Some("dedicated"), "Equal", Some("cpu"), Some("NoSchedule"))];

        assert!(!TaintToleration.filter(&CycleState::default(), &p, &n).is_success());
    }

    #[test]
    fn exists_with_an_empty_key_tolerates_every_taint() {
        // The "superuser" toleration DaemonSets use. Real API semantics, and
        // the case most likely to be missed.
        let mut n = node("tainted");
        n.taints = vec![
            taint("dedicated", "gpu", "NoSchedule"),
            taint("anything", "at-all", "NoExecute"),
        ];
        let mut p = pod("p");
        p.tolerations = vec![tol(None, "Exists", None, None)];

        assert!(TaintToleration.filter(&CycleState::default(), &p, &n).is_success());
    }

    #[test]
    fn an_empty_effect_tolerates_every_effect_of_that_key() {
        let mut n = node("tainted");
        n.taints = vec![taint("dedicated", "gpu", "NoExecute")];
        let mut p = pod("p");
        p.tolerations = vec![tol(Some("dedicated"), "Exists", None, None)];

        assert!(TaintToleration.filter(&CycleState::default(), &p, &n).is_success());
    }

    #[test]
    fn the_operator_defaults_to_equal_when_unset() {
        let t = Toleration {
            key: Some("dedicated".to_string()),
            value: Some("gpu".to_string()),
            ..Default::default()
        };
        assert!(toleration_tolerates_taint(&t, &taint("dedicated", "gpu", "NoSchedule")));
        assert!(!toleration_tolerates_taint(&t, &taint("dedicated", "cpu", "NoSchedule")));
    }

    #[test]
    fn prefer_no_schedule_never_rejects_a_node() {
        // It is a preference; rejecting on it would make it a NoSchedule.
        let mut n = node("mildly-tainted");
        n.taints = vec![taint("spot", "true", "PreferNoSchedule")];

        assert!(TaintToleration.filter(&CycleState::default(), &pod("p"), &n).is_success());
    }

    #[test]
    fn scoring_counts_only_untolerated_prefer_no_schedule_taints() {
        let mut n = node("n");
        n.taints = vec![
            taint("a", "1", "PreferNoSchedule"),
            taint("b", "2", "PreferNoSchedule"),
            // Filtered, not scored — must not be counted twice.
            taint("c", "3", "NoSchedule"),
        ];
        let mut p = pod("p");
        p.tolerations = vec![tol(Some("a"), "Exists", None, Some("PreferNoSchedule"))];

        let raw = TaintToleration.score(&CycleState::default(), &p, &n).unwrap();
        assert_eq!(raw, 1);
    }

    #[test]
    fn a_node_with_more_untolerated_preferences_scores_lower() {
        // The reversed normalization is what makes "more is worse" come out
        // as a lower final score.
        let mut scores = [0, 1, 2];
        TaintToleration.normalize(&CycleState::default(), &pod("p"), &mut scores);
        assert_eq!(scores, [100, 50, 0]);
    }

    #[test]
    fn an_untainted_cluster_scores_every_node_at_the_maximum() {
        // The degenerate branch: zero untolerated taints everywhere means
        // every node is perfect, not equally bad.
        let mut scores = [0, 0, 0];
        TaintToleration.normalize(&CycleState::default(), &pod("p"), &mut scores);
        assert_eq!(scores, [100, 100, 100]);
    }

    #[test]
    fn the_score_weight_is_three_not_one() {
        // The published docs table says 1. It is stale; default_plugins.go
        // says 3. See this module's header.
        assert_eq!(TaintToleration.weight(), 3);
    }

    #[test]
    fn it_registers_exactly_the_events_that_can_unstick_a_rejected_pod() {
        // A missing entry here is a silent five-minute stall, so this asserts
        // the exact set rather than merely that it is non-empty.
        let events = TaintToleration.events_to_register();
        let pairs: Vec<(EventResource, ActionType)> =
            events.iter().map(|e| (e.event.resource, e.event.action)).collect();

        assert_eq!(
            pairs,
            vec![
                (EventResource::Node, ActionType::ADD | ActionType::UPDATE_NODE_TAINT),
                (EventResource::Pod, ActionType::UPDATE_POD_TOLERATION),
            ]
        );
    }

    #[test]
    fn a_node_heartbeat_does_not_wake_a_pod_this_plugin_rejected() {
        // The end-to-end statement of the footprint claim, at the plugin
        // level: the subscription must not intersect a condition-only update.
        let heartbeat = ClusterEvent::new(EventResource::Node, ActionType::UPDATE_NODE_CONDITION);
        for reg in TaintToleration.events_to_register() {
            assert!(!reg.event.matches(&heartbeat));
        }
    }
}
