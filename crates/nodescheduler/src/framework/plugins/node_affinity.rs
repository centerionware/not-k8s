//! `NodeAffinity` — `nodeSelector` and `spec.affinity.nodeAffinity`.
//! Score weight **2**.
//!
//! # The boolean structure, which is not symmetric
//!
//! `nodeSelectorTerms` are **OR**ed; the `matchExpressions` within one term
//! are **AND**ed. That asymmetry is the thing to get right, and it is easy to
//! flip because the YAML nesting gives no hint of it. Flipping it does not
//! error — it just makes a multi-term affinity match far too few nodes (AND of
//! terms) or far too many (OR of expressions), and the symptom is "my pod went
//! somewhere it shouldn't have", weeks later.
//!
//! `nodeSelector` (the older, flat map) is ANDed on top of all of it.
//!
//! # Empty is not the same as absent
//!
//! An empty `nodeSelectorTerms` list matches **nothing**, while an absent
//! `nodeAffinity` matches everything. Likewise an empty `matchExpressions`
//! within a term matches everything. These are upstream's semantics and they
//! are the kind of edge that a natural implementation gets backwards.

use crate::cache::{NodeInfo, PodInfo};
use crate::events::{ActionType, ClusterEvent, EventResource};
use crate::framework::status::Status;
use crate::framework::{
    ClusterEventWithHint, CycleState, FilterPlugin, Plugin, PreScorePlugin, ScorePlugin,
};
use k8s_openapi::api::core::v1::{NodeSelector, NodeSelectorRequirement, NodeSelectorTerm};
use std::collections::BTreeMap;

pub const NAME: &str = "NodeAffinity";

/// Whether a node satisfies one requirement.
///
/// `Gt`/`Lt` compare as integers — the API restricts them to a single integer
/// value, and a node whose label is not parseable as one simply does not
/// match rather than erroring, because a malformed label is the cluster's
/// problem and must not take a scheduling cycle down.
fn matches_requirement(req: &NodeSelectorRequirement, values: &BTreeMap<String, String>) -> bool {
    let actual = values.get(&req.key);
    let wanted = req.values.clone().unwrap_or_default();

    match req.operator.as_str() {
        "In" => actual.is_some_and(|a| wanted.contains(a)),
        "NotIn" => actual.is_none_or(|a| !wanted.contains(a)),
        "Exists" => actual.is_some(),
        "DoesNotExist" => actual.is_none(),
        "Gt" | "Lt" => {
            let Some(a) = actual.and_then(|a| a.parse::<i64>().ok()) else {
                return false;
            };
            let Some(w) = wanted.first().and_then(|w| w.parse::<i64>().ok()) else {
                return false;
            };
            if req.operator == "Gt" {
                a > w
            } else {
                a < w
            }
        }
        // The apiserver validates the enum, so this is a malformed object.
        // Not matching is the safe reading.
        _ => false,
    }
}

/// Whether a node satisfies one term — expressions AND fields, all ANDed.
fn matches_term(term: &NodeSelectorTerm, node: &NodeInfo) -> bool {
    let fields: BTreeMap<String, String> =
        BTreeMap::from([("metadata.name".to_string(), node.name.clone())]);

    term.match_expressions
        .iter()
        .flatten()
        .all(|r| matches_requirement(r, &node.labels))
        && term
            .match_fields
            .iter()
            .flatten()
            .all(|r| matches_requirement(r, &fields))
}

/// Whether a node satisfies a selector — terms ORed.
pub fn matches_node_selector(selector: &NodeSelector, node: &NodeInfo) -> bool {
    // Empty terms match nothing. See the module header.
    if selector.node_selector_terms.is_empty() {
        return false;
    }
    selector.node_selector_terms.iter().any(|t| matches_term(t, node))
}

/// The flat `spec.nodeSelector` map: every entry must match exactly.
fn matches_flat_selector(selector: &BTreeMap<String, String>, node: &NodeInfo) -> bool {
    selector.iter().all(|(k, v)| node.labels.get(k) == Some(v))
}

#[derive(Default)]
pub struct NodeAffinity {
    /// A profile-wide affinity ANDed onto every pod this profile schedules.
    /// The supported way to bind a scheduler profile to a node pool without
    /// editing every workload.
    pub added_affinity: Option<NodeSelector>,
}

impl Plugin for NodeAffinity {
    fn name(&self) -> &'static str {
        NAME
    }

    fn events_to_register(&self) -> Vec<ClusterEventWithHint> {
        // Labels are what affinity matches on, so a label change or a new node
        // are the only things that can help. Deliberately not
        // UPDATE_NODE_CONDITION — that is the heartbeat bit, and subscribing
        // to it here would wake every affinity-blocked pod in the cluster
        // every ten seconds per node.
        vec![ClusterEventWithHint::always(ClusterEvent::new(
            EventResource::Node,
            ActionType::ADD | ActionType::UPDATE_NODE_LABEL,
        ))]
    }
}

impl FilterPlugin for NodeAffinity {
    fn filter(&self, _state: &CycleState, pod: &PodInfo, node: &NodeInfo) -> Status {
        if !matches_flat_selector(&pod.node_selector, node) {
            return Status::unresolvable(NAME, "node(s) didn't match Pod's node affinity/selector");
        }
        if let Some(required) = pod.node_affinity.as_deref() {
            if !matches_node_selector(required, node) {
                return Status::unresolvable(
                    NAME,
                    "node(s) didn't match Pod's node affinity/selector",
                );
            }
        }
        if let Some(added) = self.added_affinity.as_ref() {
            if !matches_node_selector(added, node) {
                return Status::unresolvable(NAME, "node(s) didn't match the profile's addedAffinity");
            }
        }
        Status::success()
    }
}

impl PreScorePlugin for NodeAffinity {
    fn pre_score(&self, state: &mut CycleState, pod: &PodInfo, _nodes: &[&NodeInfo]) -> Status {
        // A pod expressing no preference scores every node identically, so
        // running it per node is pure waste.
        if pod.preferred_node_affinity.is_empty() {
            state.skip_score(NAME);
            return Status::skip();
        }
        Status::success()
    }
}

impl ScorePlugin for NodeAffinity {
    fn score(&self, _state: &CycleState, pod: &PodInfo, node: &NodeInfo) -> Result<i64, Status> {
        Ok(pod
            .preferred_node_affinity
            .iter()
            .filter(|t| matches_term(&t.selector, node))
            .map(|t| t.weight as i64)
            .sum())
    }

    fn normalize(&self, _state: &CycleState, _pod: &PodInfo, scores: &mut [i64]) -> Status {
        super::default_normalize_score(false, scores);
        Status::success()
    }

    fn weight(&self) -> i64 {
        2
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::PreferredTerm;
    use crate::framework::plugins::testutil::{node, pod};

    fn req(key: &str, op: &str, values: &[&str]) -> NodeSelectorRequirement {
        NodeSelectorRequirement {
            key: key.to_string(),
            operator: op.to_string(),
            values: Some(values.iter().map(|s| s.to_string()).collect()),
        }
    }

    fn term(reqs: Vec<NodeSelectorRequirement>) -> NodeSelectorTerm {
        NodeSelectorTerm { match_expressions: Some(reqs), match_fields: None }
    }

    fn labelled(name: &str, labels: &[(&str, &str)]) -> NodeInfo {
        let mut n = node(name);
        n.labels = labels.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        n
    }

    #[test]
    fn a_flat_node_selector_must_match_every_entry() {
        let mut p = pod("p");
        p.node_selector = BTreeMap::from([
            ("disk".to_string(), "ssd".to_string()),
            ("zone".to_string(), "east".to_string()),
        ]);

        let both = labelled("a", &[("disk", "ssd"), ("zone", "east")]);
        let one = labelled("b", &[("disk", "ssd")]);

        assert!(NodeAffinity::default().filter(&CycleState::default(), &p, &both).is_success());
        assert!(!NodeAffinity::default().filter(&CycleState::default(), &p, &one).is_success());
    }

    #[test]
    fn terms_are_ored_and_expressions_within_a_term_are_anded() {
        // The asymmetry from the module header, stated as a test because
        // flipping it is silent.
        let selector = NodeSelector {
            node_selector_terms: vec![
                term(vec![req("disk", "In", &["ssd"]), req("zone", "In", &["east"])]),
                term(vec![req("disk", "In", &["nvme"])]),
            ],
        };
        let mut p = pod("p");
        p.node_affinity = Some(Box::new(selector));
        let plugin = NodeAffinity::default();

        // Satisfies the whole first term.
        let a = labelled("a", &[("disk", "ssd"), ("zone", "east")]);
        assert!(plugin.filter(&CycleState::default(), &p, &a).is_success());

        // Satisfies the second term alone — OR means that is enough.
        let b = labelled("b", &[("disk", "nvme")]);
        assert!(plugin.filter(&CycleState::default(), &p, &b).is_success());

        // Half of the first term and none of the second — AND means not enough.
        let c = labelled("c", &[("disk", "ssd")]);
        assert!(!plugin.filter(&CycleState::default(), &p, &c).is_success());
    }

    #[test]
    fn an_empty_term_list_matches_nothing() {
        // Empty is not absent. Getting this backwards makes a pod with a
        // vacuous affinity schedulable anywhere.
        let mut p = pod("p");
        p.node_affinity = Some(Box::new(NodeSelector { node_selector_terms: vec![] }));

        assert!(!NodeAffinity::default()
            .filter(&CycleState::default(), &p, &node("any"))
            .is_success());
    }

    #[test]
    fn an_absent_affinity_matches_everything() {
        assert!(NodeAffinity::default()
            .filter(&CycleState::default(), &pod("p"), &node("any"))
            .is_success());
    }

    #[test]
    fn every_operator_behaves_as_the_api_defines_it() {
        let labels = BTreeMap::from([
            ("present".to_string(), "yes".to_string()),
            ("count".to_string(), "5".to_string()),
        ]);

        assert!(matches_requirement(&req("present", "In", &["yes", "no"]), &labels));
        assert!(!matches_requirement(&req("present", "In", &["no"]), &labels));

        assert!(matches_requirement(&req("present", "NotIn", &["no"]), &labels));
        assert!(!matches_requirement(&req("present", "NotIn", &["yes"]), &labels));

        assert!(matches_requirement(&req("present", "Exists", &[]), &labels));
        assert!(!matches_requirement(&req("absent", "Exists", &[]), &labels));

        assert!(matches_requirement(&req("absent", "DoesNotExist", &[]), &labels));
        assert!(!matches_requirement(&req("present", "DoesNotExist", &[]), &labels));

        assert!(matches_requirement(&req("count", "Gt", &["3"]), &labels));
        assert!(!matches_requirement(&req("count", "Gt", &["9"]), &labels));
        assert!(matches_requirement(&req("count", "Lt", &["9"]), &labels));
    }

    #[test]
    fn notin_is_satisfied_by_a_label_that_is_absent_entirely() {
        // Upstream semantics, and the one operator whose "missing" case is
        // not simply false.
        let labels = BTreeMap::new();
        assert!(matches_requirement(&req("anything", "NotIn", &["x"]), &labels));
    }

    #[test]
    fn a_non_numeric_label_never_satisfies_gt_or_lt() {
        // A malformed label must not take the cycle down.
        let labels = BTreeMap::from([("count".to_string(), "many".to_string())]);
        assert!(!matches_requirement(&req("count", "Gt", &["3"]), &labels));
    }

    #[test]
    fn match_fields_can_select_a_node_by_name() {
        let selector = NodeSelector {
            node_selector_terms: vec![NodeSelectorTerm {
                match_expressions: None,
                match_fields: Some(vec![req("metadata.name", "In", &["worker-3"])]),
            }],
        };
        let mut p = pod("p");
        p.node_affinity = Some(Box::new(selector));
        let plugin = NodeAffinity::default();

        assert!(plugin.filter(&CycleState::default(), &p, &node("worker-3")).is_success());
        assert!(!plugin.filter(&CycleState::default(), &p, &node("worker-4")).is_success());
    }

    #[test]
    fn an_affinity_rejection_is_unresolvable_by_preemption() {
        // Evicting pods cannot change a node's labels, so preemption must not
        // be invited to try.
        let mut p = pod("p");
        p.node_selector = BTreeMap::from([("disk".to_string(), "ssd".to_string())]);

        let s = NodeAffinity::default().filter(&CycleState::default(), &p, &node("plain"));
        assert!(!s.code.is_resolvable_by_preemption());
    }

    #[test]
    fn the_profiles_added_affinity_is_anded_onto_every_pod() {
        let plugin = NodeAffinity {
            added_affinity: Some(NodeSelector {
                node_selector_terms: vec![term(vec![req("pool", "In", &["batch"])])],
            }),
        };
        let p = pod("p");

        assert!(plugin
            .filter(&CycleState::default(), &p, &labelled("a", &[("pool", "batch")]))
            .is_success());
        assert!(!plugin
            .filter(&CycleState::default(), &p, &labelled("b", &[("pool", "web")]))
            .is_success());
    }

    #[test]
    fn preferred_terms_sum_their_weights() {
        let mut p = pod("p");
        p.preferred_node_affinity = vec![
            PreferredTerm { weight: 10, selector: term(vec![req("disk", "In", &["ssd"])]) },
            PreferredTerm { weight: 5, selector: term(vec![req("zone", "In", &["east"])]) },
        ];
        let plugin = NodeAffinity::default();
        let state = CycleState::default();

        let both = labelled("both", &[("disk", "ssd"), ("zone", "east")]);
        let one = labelled("one", &[("disk", "ssd")]);
        let neither = node("neither");

        assert_eq!(plugin.score(&state, &p, &both).unwrap(), 15);
        assert_eq!(plugin.score(&state, &p, &one).unwrap(), 10);
        assert_eq!(plugin.score(&state, &p, &neither).unwrap(), 0);
    }

    #[test]
    fn a_pod_with_no_preferences_skips_scoring() {
        let mut state = CycleState::default();
        let s = NodeAffinity::default().pre_score(&mut state, &pod("p"), &[]);

        assert!(s.is_skip());
        assert!(state.score_skipped(NAME));
    }

    #[test]
    fn the_score_weight_is_two() {
        assert_eq!(NodeAffinity::default().weight(), 2);
    }

    #[test]
    fn it_does_not_subscribe_to_node_heartbeats() {
        // Subscribing to condition changes here would wake every
        // affinity-blocked pod every ten seconds per node.
        let heartbeat = ClusterEvent::new(EventResource::Node, ActionType::UPDATE_NODE_CONDITION);
        for reg in NodeAffinity::default().events_to_register() {
            assert!(!reg.event.matches(&heartbeat));
        }
    }

    #[test]
    fn it_wakes_on_a_node_label_change() {
        let relabel = ClusterEvent::new(EventResource::Node, ActionType::UPDATE_NODE_LABEL);
        assert!(NodeAffinity::default()
            .events_to_register()
            .iter()
            .any(|e| e.event.matches(&relabel)));
    }
}
