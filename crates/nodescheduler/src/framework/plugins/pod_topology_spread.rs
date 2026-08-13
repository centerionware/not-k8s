//! `PodTopologySpread` — keep a workload's pods spread evenly across failure
//! domains. Score weight **2**.
//!
//! # Skew, and why `minDomains` exists
//!
//! Each constraint names a `topologyKey`; nodes sharing a value for it form a
//! domain. **Skew** is `count(this domain) - globalMin`, where `globalMin` is
//! the smallest matching-pod count across all *eligible* domains. Placing a
//! pod is allowed when the resulting skew stays within `maxSkew`.
//!
//! The subtlety is what `globalMin` means on a cluster that has not filled
//! out yet. With one zone occupied and two empty, `globalMin` is 0, so the
//! occupied zone is already at its skew limit and new pods go elsewhere —
//! which is the point. But if only *one* domain exists at all, `globalMin` is
//! that domain's own count, skew is always 0, and the constraint silently does
//! nothing. `minDomains` is the guard: when fewer than `minDomains` eligible
//! domains exist, `globalMin` is forced to 0, so the constraint keeps biting
//! until the cluster is wide enough to satisfy it.
//!
//! # `whenUnsatisfiable` decides whether this is a filter or a score
//!
//!   * `DoNotSchedule` — a hard constraint. Rejects the node.
//!   * `ScheduleAnyway` — a preference. Contributes to the score only.
//!
//! Both are evaluated from the same counts, which is why they live in one
//! plugin, and mixing them up turns a hint into an outage or an outage into a
//! hint.
//!
//! # Eligibility, which is not just "has the label"
//!
//! A domain participates only if its nodes pass the pod's own placement
//! rules, per two policies that went GA in v1.33:
//!
//!   * `nodeAffinityPolicy: Honor` (**default**) — only nodes matching the
//!     pod's `nodeSelector`/`nodeAffinity` count. Otherwise a zone the pod
//!     could never be placed in drags `globalMin` to zero and blocks
//!     everything.
//!   * `nodeTaintsPolicy: Ignore` (**default**) — taints disregarded unless
//!     asked otherwise.
//!
//! # What is not implemented here
//!
//! **System default constraints.** With `defaultingType: SystemDefaulting`
//! upstream applies built-in constraints (maxSkew 3 across zones, 5 across
//! hosts) to pods that declare none, deriving the label selector from the
//! pod's owning Service/ReplicaSet/ReplicationController/StatefulSet — which
//! needs four informers whose only consumer is this feature.
//!
//! They are `ScheduleAnyway`, so their absence changes **scores, never
//! feasibility**: no pod is placed that upstream would have refused, and none
//! is refused that upstream would have placed. `NODESCHEDULER_TOPOLOGY_
//! DEFAULTING` already exists to select the behaviour, and the gap is stated
//! in docs/SCHEDULER.md rather than left for someone to discover from a
//! scoring difference.

use super::node_affinity::matches_node_selector;
use super::selector::matches_selector;
use super::taint_toleration::find_untolerated_taint;
use crate::cache::{NodeInfo, PodInfo, Snapshot};
use crate::events::{ActionType, ClusterEvent, EventResource};
use crate::framework::status::Status;
use crate::framework::{
    ClusterEventWithHint, CycleState, FilterPlugin, Plugin, PreFilterExtensions, PreFilterPlugin,
    PreScorePlugin, ScorePlugin, MAX_NODE_SCORE,
};
use k8s_openapi::api::core::v1::TopologySpreadConstraint;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{LabelSelector, LabelSelectorRequirement};
use std::collections::HashMap;

pub const NAME: &str = "PodTopologySpread";

/// One constraint, with its per-domain counts resolved.
#[derive(Clone)]
struct ResolvedConstraint {
    max_skew: i32,
    topology_key: String,
    /// `DoNotSchedule` (hard) or `ScheduleAnyway` (scoring only).
    hard: bool,
    min_domains: i32,
    selector: Option<LabelSelector>,
    /// Domain value -> matching pods currently in it. Only eligible domains
    /// appear; an ineligible one must not drag `globalMin` down.
    counts: HashMap<String, i64>,
}

impl ResolvedConstraint {
    /// The smallest count across eligible domains, with the `minDomains`
    /// guard applied. See the module header.
    fn global_min(&self) -> i64 {
        if (self.counts.len() as i32) < self.min_domains {
            return 0;
        }
        self.counts.values().copied().min().unwrap_or(0)
    }
}

#[derive(Clone, Default)]
struct SpreadState {
    constraints: Vec<ResolvedConstraint>,
}

#[derive(Default)]
pub struct PodTopologySpread;

/// `matchLabelKeys` folded into the selector.
///
/// At v1.33 this is the **scheduler's** job (it moved to the apiserver in
/// v1.34): for each key, look up the incoming pod's own value and AND
/// `key=value` into the selector. It is how a Deployment rollout spreads each
/// revision independently rather than counting the old revision's pods as its
/// own — usually via `pod-template-hash`.
///
/// A key the pod does not carry is skipped, per upstream.
fn effective_selector(constraint: &TopologySpreadConstraint, pod: &PodInfo) -> Option<LabelSelector> {
    let base = constraint.label_selector.clone()?;
    let keys = constraint.match_label_keys.clone().unwrap_or_default();
    if keys.is_empty() {
        return Some(base);
    }

    let mut sel = base;
    let mut extra: Vec<LabelSelectorRequirement> = sel.match_expressions.clone().unwrap_or_default();
    for key in keys {
        let Some(value) = pod.labels.get(&key) else {
            continue;
        };
        extra.push(LabelSelectorRequirement {
            key,
            operator: "In".to_string(),
            values: Some(vec![value.clone()]),
        });
    }
    sel.match_expressions = Some(extra);
    Some(sel)
}

/// Whether a node's domain may participate, per the two inclusion policies.
fn node_is_eligible(constraint: &TopologySpreadConstraint, pod: &PodInfo, node: &NodeInfo) -> bool {
    // Honor is the default for affinity: a domain the pod could never be
    // placed in must not drag globalMin to zero and block every other domain.
    let honor_affinity = constraint
        .node_affinity_policy
        .as_deref()
        .unwrap_or("Honor")
        == "Honor";
    if honor_affinity {
        if !pod.node_selector.iter().all(|(k, v)| node.labels.get(k) == Some(v)) {
            return false;
        }
        if let Some(required) = pod.node_affinity.as_deref() {
            if !matches_node_selector(required, node) {
                return false;
            }
        }
    }

    // Ignore is the default for taints.
    let honor_taints = constraint
        .node_taints_policy
        .as_deref()
        .unwrap_or("Ignore")
        == "Honor";
    if honor_taints
        && find_untolerated_taint(&node.taints, &pod.tolerations, &["NoSchedule", "NoExecute"])
            .is_some()
    {
        return false;
    }

    true
}

impl Plugin for PodTopologySpread {
    fn name(&self) -> &'static str {
        NAME
    }

    fn events_to_register(&self) -> Vec<ClusterEventWithHint> {
        vec![
            // Counts change when a matching pod appears, leaves, or is
            // relabelled into or out of the selector.
            ClusterEventWithHint::always(ClusterEvent::new(
                EventResource::AssignedPod,
                ActionType::ADD | ActionType::DELETE | ActionType::UPDATE_POD_LABEL,
            )),
            // A node's labels place it in a domain; its taints can make it
            // ineligible under Honor. A new node may create an empty domain,
            // which changes globalMin for everyone.
            ClusterEventWithHint::always(ClusterEvent::new(
                EventResource::Node,
                ActionType::ADD
                    | ActionType::DELETE
                    | ActionType::UPDATE_NODE_LABEL
                    | ActionType::UPDATE_NODE_TAINT,
            )),
        ]
    }
}

impl PreFilterPlugin for PodTopologySpread {
    fn pre_filter(
        &self,
        state: &mut CycleState,
        pod: &PodInfo,
        snapshot: &Snapshot,
    ) -> (Status, Option<Vec<String>>) {
        if pod.topology_spread_constraints.is_empty() {
            // System default constraints would apply here; see the module
            // header for why their absence is a scoring difference only.
            state.skip_filter(NAME);
            state.skip_score(NAME);
            return (Status::skip(), None);
        }

        let mut resolved = Vec::new();
        for c in &pod.topology_spread_constraints {
            let selector = effective_selector(c, pod);
            let hard = c.when_unsatisfiable == "DoNotSchedule";
            let mut counts: HashMap<String, i64> = HashMap::new();

            for node in snapshot.nodes() {
                let Some(value) = node.labels.get(&c.topology_key) else {
                    // No label for this key: the node is in no domain at all
                    // and is invisible to this constraint.
                    continue;
                };
                if !node_is_eligible(c, pod, node) {
                    continue;
                }
                // Every eligible domain is registered even at zero, because an
                // empty domain is exactly what pulls globalMin down and forces
                // the spread.
                let entry = counts.entry(value.clone()).or_insert(0);
                for existing in &node.pods {
                    if existing.namespace == pod.namespace
                        && matches_selector(selector.as_ref(), &existing.labels)
                    {
                        *entry += 1;
                    }
                }
            }

            resolved.push(ResolvedConstraint {
                max_skew: c.max_skew.max(1),
                topology_key: c.topology_key.clone(),
                hard,
                min_domains: c.min_domains.unwrap_or(1),
                selector,
                counts,
            });
        }

        // A hard constraint with no eligible domain at all can never be
        // satisfied by evicting anything, so preemption must not be invited
        // to try.
        for c in &resolved {
            if c.hard && c.counts.is_empty() {
                state.write(NAME, SpreadState { constraints: resolved.clone() });
                return (
                    Status::unresolvable(
                        NAME,
                        format!("no eligible domain for topology key {}", c.topology_key),
                    ),
                    None,
                );
            }
        }

        state.write(NAME, SpreadState { constraints: resolved });
        (Status::success(), None)
    }

    fn extensions(&self) -> Option<&dyn PreFilterExtensions> {
        Some(self)
    }
}

impl PreFilterExtensions for PodTopologySpread {
    fn add_pod(
        &self,
        state: &mut CycleState,
        pod: &PodInfo,
        pod_to_add: &PodInfo,
        node: &NodeInfo,
    ) -> Status {
        adjust(state, pod, pod_to_add, node, 1);
        Status::success()
    }

    fn remove_pod(
        &self,
        state: &mut CycleState,
        pod: &PodInfo,
        pod_to_remove: &PodInfo,
        node: &NodeInfo,
    ) -> Status {
        adjust(state, pod, pod_to_remove, node, -1);
        Status::success()
    }
}

fn adjust(state: &mut CycleState, pod: &PodInfo, other: &PodInfo, node: &NodeInfo, sign: i64) {
    let Some(mut s) = state.read::<SpreadState>(NAME).cloned() else {
        return;
    };
    for c in s.constraints.iter_mut() {
        let Some(value) = node.labels.get(&c.topology_key) else {
            continue;
        };
        if other.namespace != pod.namespace {
            continue;
        }
        if !matches_selector(c.selector.as_ref(), &other.labels) {
            continue;
        }
        // Only an already-eligible domain is adjusted. A victim on an
        // ineligible node changes nothing, and inventing a domain entry here
        // would let preemption believe it had widened the cluster.
        if let Some(entry) = c.counts.get_mut(value) {
            *entry += sign;
        }
    }
    state.write(NAME, s);
}

impl FilterPlugin for PodTopologySpread {
    fn filter(&self, state: &CycleState, _pod: &PodInfo, node: &NodeInfo) -> Status {
        let Some(s) = state.read::<SpreadState>(NAME) else {
            return Status::success();
        };

        for c in &s.constraints {
            if !c.hard {
                continue;
            }
            let Some(value) = node.labels.get(&c.topology_key) else {
                return Status::unschedulable(
                    NAME,
                    format!("node(s) didn't have label {}", c.topology_key),
                );
            };
            let Some(current) = c.counts.get(value) else {
                // The node is in no *eligible* domain for this constraint.
                return Status::unschedulable(
                    NAME,
                    "node(s) didn't match pod topology spread constraints",
                );
            };

            // The skew this placement would produce.
            let skew = (current + 1) - c.global_min();
            if skew > c.max_skew as i64 {
                return Status::unschedulable(
                    NAME,
                    "node(s) didn't satisfy pod topology spread constraints (maxSkew exceeded)",
                );
            }
        }
        Status::success()
    }
}

impl PreScorePlugin for PodTopologySpread {
    fn pre_score(&self, state: &mut CycleState, _pod: &PodInfo, _nodes: &[&NodeInfo]) -> Status {
        let scorable = state
            .read::<SpreadState>(NAME)
            .map(|s| s.constraints.iter().any(|c| !c.hard))
            .unwrap_or(false);
        if !scorable {
            state.skip_score(NAME);
            return Status::skip();
        }
        Status::success()
    }
}

/// Upstream's `topologyNormalizingWeight`: `ln(domains + 2)`.
///
/// Larger topologies count for more, so a constraint over many zones is not
/// drowned out by one over two. Logarithmic rather than linear so the effect
/// saturates: each *additional* domain is worth less than the one before it,
/// because the derivative of `ln(n+2)` is `1/(n+2)`.
///
/// Stated as marginal cost deliberately. Comparing unequal spans says the
/// opposite and sounds just as plausible — going from 300 to 400 domains is a
/// bigger jump than 3 to 4, precisely because it is a hundred domains rather
/// than one. The first version of this comment made that mistake, and so did
/// the test asserting it.
fn topology_normalizing_weight(domains: usize) -> f64 {
    ((domains + 2) as f64).ln()
}

impl ScorePlugin for PodTopologySpread {
    fn score(&self, state: &CycleState, _pod: &PodInfo, node: &NodeInfo) -> Result<i64, Status> {
        let Some(s) = state.read::<SpreadState>(NAME) else {
            return Ok(0);
        };

        // Raw score counts *occupancy*, so lower is better here; `normalize`
        // inverts it. Keeping the raw score in the natural direction is what
        // makes the formula match upstream's line for line.
        let mut raw = 0f64;
        for c in &s.constraints {
            if c.hard {
                continue;
            }
            let Some(value) = node.labels.get(&c.topology_key) else {
                continue;
            };
            let Some(count) = c.counts.get(value) else {
                continue;
            };
            let weight = topology_normalizing_weight(c.counts.len());
            raw += *count as f64 * weight + (c.max_skew as f64 - 1.0);
        }
        Ok(raw.round() as i64)
    }

    fn normalize(&self, _state: &CycleState, _pod: &PodInfo, scores: &mut [i64]) -> Status {
        // Inverted: the emptiest domain scores highest. Upstream's shape is
        // `MAX * (max + min - s) / max`, which keeps the *best* node at the
        // maximum rather than merely at the top of whatever range appeared.
        let max = scores.iter().copied().max().unwrap_or(0);
        let min = scores.iter().copied().min().unwrap_or(0);
        if max <= 0 {
            for s in scores.iter_mut() {
                *s = MAX_NODE_SCORE;
            }
            return Status::success();
        }
        for s in scores.iter_mut() {
            *s = (MAX_NODE_SCORE * (max + min - *s) / max).clamp(0, MAX_NODE_SCORE);
        }
        Status::success()
    }

    fn weight(&self) -> i64 {
        2
    }
}

#[cfg(test)]
#[path = "pod_topology_spread_tests.rs"]
mod tests;
