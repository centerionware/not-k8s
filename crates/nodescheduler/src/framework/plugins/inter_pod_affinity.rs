//! `InterPodAffinity` — place a pod near, or away from, other pods.
//! Score weight **2**.
//!
//! # Topology domains, not nodes
//!
//! Every rule here is about a *domain*, not a node: `topologyKey` names a node
//! label, and all nodes sharing a value for it form one domain. "Anti-affinity
//! on `kubernetes.io/hostname`" means one pod per node; the same rule on
//! `topology.kubernetes.io/zone` means one pod per zone, which is a far
//! stronger and far more expensive statement.
//!
//! Satisfaction is asymmetric and easy to invert:
//!
//!   * **affinity** is satisfied when **at least one** matching pod is
//!     already in the candidate node's domain;
//!   * **anti-affinity** is satisfied when **no** matching pod is.
//!
//! A node whose `topologyKey` label is missing is in no domain, so it
//! satisfies neither — it cannot be shown to have a matching neighbour, and it
//! cannot be shown not to.
//!
//! # Why the counting happens in PreFilter
//!
//! `Filter` is handed one node. A domain spans nodes, so the question "how
//! many matching pods are in this node's domain" cannot be answered from the
//! node alone — and threading the snapshot into `Filter` would answer it
//! once per node per cycle, which is the quadratic behaviour upstream
//! documents as this plugin's scalability caveat.
//!
//! So PreFilter walks the cluster **once** and builds three maps keyed by
//! `topologyKey=value`, and Filter does three lookups. That is upstream's
//! design, and it is the reason `Filter`'s signature does not need the
//! snapshot.
//!
//! # The third map is the one people forget
//!
//! Affinity is symmetric in a way anti-affinity makes dangerous. Two rules
//! must hold before a pod may be placed:
//!
//!   1. the incoming pod's own rules about existing pods, and
//!   2. **existing pods' rules about the incoming pod** — an already-running
//!      pod with `podAntiAffinity` matching our labels must not have us
//!      dropped into its domain.
//!
//! Checking only (1) lets a new pod violate a constraint a running pod
//! declared, which is invisible until something is evicted or a rollout
//! mysteriously refuses to converge. `existing_anti_affinity` is (2).

use super::selector::{matches_selector, namespace_in_scope, namespace_scope};
use crate::cache::{NodeInfo, PodInfo, Snapshot};
use crate::events::{ActionType, ClusterEvent, EventResource};
use crate::framework::status::Status;
use crate::framework::{
    ClusterEventWithHint, CycleState, FilterPlugin, Plugin, PreFilterExtensions, PreFilterPlugin,
    PreScorePlugin, ScorePlugin, MAX_NODE_SCORE,
};
use k8s_openapi::api::core::v1::{PodAffinityTerm, WeightedPodAffinityTerm};
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

pub const NAME: &str = "InterPodAffinity";

/// `topologyKey=value`. One string rather than a tuple so it is a cheap
/// HashMap key and prints legibly in a rejection message.
type DomainKey = String;

fn domain_key(topology_key: &str, value: &str) -> DomainKey {
    format!("{topology_key}={value}")
}

/// Counts computed once per cycle, then read per node.
#[derive(Clone, Default)]
struct AffinityState {
    required_affinity: Vec<PodAffinityTerm>,
    required_anti_affinity: Vec<PodAffinityTerm>,
    preferred_affinity: Vec<WeightedPodAffinityTerm>,
    preferred_anti_affinity: Vec<WeightedPodAffinityTerm>,

    /// Per domain: existing pods matching the incoming pod's affinity terms.
    affinity_counts: HashMap<DomainKey, i64>,
    /// Per domain: existing pods matching its anti-affinity terms.
    anti_affinity_counts: HashMap<DomainKey, i64>,
    /// Per domain: existing pods whose *own* required anti-affinity matches
    /// the incoming pod. See the module header — this is the symmetric half.
    existing_anti_affinity: HashMap<DomainKey, i64>,
    /// Per domain, per preferred term index: matching pods, for scoring.
    preferred_affinity_counts: HashMap<DomainKey, i64>,
    preferred_anti_affinity_counts: HashMap<DomainKey, i64>,

    /// Namespace labels, for terms using `namespaceSelector`.
    ///
    /// Carried in the cycle state rather than read from the snapshot at use
    /// time because `PreFilterExtensions` — the add/remove-pod hooks
    /// preemption drives — are handed no snapshot. `Arc`, so the per-cycle
    /// clone is a refcount bump and not a copy of every namespace.
    namespace_labels: Arc<HashMap<String, BTreeMap<String, String>>>,
}

impl AffinityState {
    fn has_required(&self) -> bool {
        !self.required_affinity.is_empty() || !self.required_anti_affinity.is_empty()
    }
    fn has_preferred(&self) -> bool {
        !self.preferred_affinity.is_empty() || !self.preferred_anti_affinity.is_empty()
    }
}

pub struct InterPodAffinity {
    /// Weight given to an *existing* pod's required affinity when scoring —
    /// the symmetry term. Required terms carry no weight of their own, so
    /// upstream applies this flat value. Default 1.
    pub hard_pod_affinity_weight: i32,
}

impl Default for InterPodAffinity {
    fn default() -> Self {
        Self { hard_pod_affinity_weight: 1 }
    }
}

/// Whether an existing pod matches a term, from the perspective of a pod in
/// `own_namespace`.
fn pod_matches_term(
    term: &PodAffinityTerm,
    existing: &PodInfo,
    own_namespace: &str,
    namespace_labels: &HashMap<String, BTreeMap<String, String>>,
) -> bool {
    let scope = namespace_scope(term.namespaces.as_ref(), term.namespace_selector.as_ref());
    if !namespace_in_scope(&scope, own_namespace, &existing.namespace, namespace_labels) {
        return false;
    }
    matches_selector(term.label_selector.as_ref(), &existing.labels)
}

/// Add one to every domain in which `node` sits for the given terms, when the
/// existing pod matches.
fn tally(
    counts: &mut HashMap<DomainKey, i64>,
    terms: &[PodAffinityTerm],
    existing: &PodInfo,
    node: &NodeInfo,
    own_namespace: &str,
    namespace_labels: &HashMap<String, BTreeMap<String, String>>,
    sign: i64,
) {
    for term in terms {
        let Some(value) = node.labels.get(&term.topology_key) else {
            continue;
        };
        if pod_matches_term(term, existing, own_namespace, namespace_labels) {
            *counts.entry(domain_key(&term.topology_key, value)).or_insert(0) += sign;
        }
    }
}

/// An existing pod's own required anti-affinity terms.
fn required_anti_affinity_of(pod: &PodInfo) -> Vec<PodAffinityTerm> {
    pod.affinity
        .as_deref()
        .and_then(|a| a.pod_anti_affinity.as_ref())
        .and_then(|aa| aa.required_during_scheduling_ignored_during_execution.clone())
        .unwrap_or_default()
}

impl Plugin for InterPodAffinity {
    fn name(&self) -> &'static str {
        NAME
    }

    fn events_to_register(&self) -> Vec<ClusterEventWithHint> {
        vec![
            // A pod appearing or leaving changes what is in a domain, and so
            // can satisfy an affinity or release an anti-affinity. Its labels
            // changing does the same without it moving.
            ClusterEventWithHint::always(ClusterEvent::new(
                EventResource::AssignedPod,
                ActionType::ADD | ActionType::DELETE | ActionType::UPDATE_POD_LABEL,
            )),
            // A node's labels decide which domain it is in, so relabelling
            // moves it between domains; a new node may be in an empty one.
            ClusterEventWithHint::always(ClusterEvent::new(
                EventResource::Node,
                ActionType::ADD | ActionType::UPDATE_NODE_LABEL,
            )),
        ]
    }
}

impl PreFilterPlugin for InterPodAffinity {
    fn pre_filter(
        &self,
        state: &mut CycleState,
        pod: &PodInfo,
        snapshot: &Snapshot,
    ) -> (Status, Option<Vec<String>>) {
        let affinity = pod.affinity.as_deref();
        let mut s = AffinityState {
            required_affinity: affinity
                .and_then(|a| a.pod_affinity.as_ref())
                .and_then(|a| a.required_during_scheduling_ignored_during_execution.clone())
                .unwrap_or_default(),
            required_anti_affinity: affinity
                .and_then(|a| a.pod_anti_affinity.as_ref())
                .and_then(|a| a.required_during_scheduling_ignored_during_execution.clone())
                .unwrap_or_default(),
            preferred_affinity: affinity
                .and_then(|a| a.pod_affinity.as_ref())
                .and_then(|a| a.preferred_during_scheduling_ignored_during_execution.clone())
                .unwrap_or_default(),
            preferred_anti_affinity: affinity
                .and_then(|a| a.pod_anti_affinity.as_ref())
                .and_then(|a| a.preferred_during_scheduling_ignored_during_execution.clone())
                .unwrap_or_default(),
            namespace_labels: snapshot.namespaces.clone(),
            ..Default::default()
        };

        // The symmetric half applies to *every* pod, even one with no affinity
        // rules of its own: a running pod's anti-affinity can still forbid
        // placing us next to it. So this is computed before the early-out.
        let preferred_affinity_terms: Vec<PodAffinityTerm> =
            s.preferred_affinity.iter().map(|w| w.pod_affinity_term.clone()).collect();
        let preferred_anti_terms: Vec<PodAffinityTerm> =
            s.preferred_anti_affinity.iter().map(|w| w.pod_affinity_term.clone()).collect();
        // Held separately so the tally loop can borrow `s`'s count maps
        // mutably while still reading the labels. Refcount bump, not a copy.
        let ns_labels = s.namespace_labels.clone();

        // Only pods that declared anti-affinity can forbid anything, and the
        // snapshot keeps the *nodes carrying any of them* pre-filtered too —
        // walking every node in the cluster to find the handful that matter
        // is exactly the cost this subset exists to avoid.
        for node in &snapshot.nodes_with_pods_with_required_anti_affinity {
            for existing in &node.pods_with_required_anti_affinity {
                let terms = required_anti_affinity_of(existing);
                for term in &terms {
                    let Some(value) = node.labels.get(&term.topology_key) else {
                        continue;
                    };
                    // Their rule, our labels — the direction that is easy to
                    // write backwards.
                    let scope =
                        namespace_scope(term.namespaces.as_ref(), term.namespace_selector.as_ref());
                    if namespace_in_scope(
                        &scope,
                        &existing.namespace,
                        &pod.namespace,
                        &s.namespace_labels,
                    ) && matches_selector(term.label_selector.as_ref(), &pod.labels)
                    {
                        *s.existing_anti_affinity
                            .entry(domain_key(&term.topology_key, value))
                            .or_insert(0) += 1;
                    }
                }
            }
        }

        // The tally loop below counts *our own* rules against every existing
        // pod in the cluster, so — unlike the symmetric pass above — no
        // pre-filtered node subset can stand in for "every node": a pod
        // matching our term is not tagged as such ahead of time. But a pod
        // declaring no affinity/anti-affinity terms of its own (the
        // overwhelming majority) has nothing for that walk to find either
        // way, so skip it rather than pay for an O(every pod in the cluster)
        // scan whose every `tally()` call would immediately return on an
        // empty term list anyway.
        if s.has_required() || s.has_preferred() {
            for node in snapshot.nodes() {
                for existing in &node.pods {
                    tally(
                        &mut s.affinity_counts,
                        &s.required_affinity,
                        existing,
                        node,
                        &pod.namespace,
                        &ns_labels,
                        1,
                    );
                    tally(
                        &mut s.anti_affinity_counts,
                        &s.required_anti_affinity,
                        existing,
                        node,
                        &pod.namespace,
                        &ns_labels,
                        1,
                    );
                    tally(
                        &mut s.preferred_affinity_counts,
                        &preferred_affinity_terms,
                        existing,
                        node,
                        &pod.namespace,
                        &ns_labels,
                        1,
                    );
                    tally(
                        &mut s.preferred_anti_affinity_counts,
                        &preferred_anti_terms,
                        existing,
                        node,
                        &pod.namespace,
                        &ns_labels,
                        1,
                    );
                }
            }
        }

        if !s.has_required() && s.existing_anti_affinity.is_empty() {
            state.skip_filter(NAME);
        }
        if !s.has_preferred() {
            state.skip_score(NAME);
        }

        let nothing_to_do =
            !s.has_required() && !s.has_preferred() && s.existing_anti_affinity.is_empty();
        state.write(NAME, s);

        if nothing_to_do {
            // The overwhelming majority of pods, on a cluster using none of
            // this. Skipping keeps the whole plugin off the common path.
            return (Status::skip(), None);
        }
        (Status::success(), None)
    }

    fn extensions(&self) -> Option<&dyn PreFilterExtensions> {
        Some(self)
    }
}

impl PreFilterExtensions for InterPodAffinity {
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

/// Preemption is pretending a pod joined or left `node`; adjust every count it
/// contributes to.
///
/// All five maps, not just the obvious two. A victim can be the pod that was
/// satisfying our affinity, the pod our anti-affinity was avoiding, *or* the
/// pod whose own anti-affinity was forbidding us — and removing it changes a
/// different answer in each case. Missing the third means preemption evicts a
/// pod and still refuses to place the preemptor.
fn adjust(state: &mut CycleState, pod: &PodInfo, other: &PodInfo, node: &NodeInfo, sign: i64) {
    let Some(mut s) = state.read::<AffinityState>(NAME).cloned() else {
        return;
    };

    let required_affinity = s.required_affinity.clone();
    let required_anti = s.required_anti_affinity.clone();
    let preferred_affinity: Vec<PodAffinityTerm> =
        s.preferred_affinity.iter().map(|w| w.pod_affinity_term.clone()).collect();
    let preferred_anti: Vec<PodAffinityTerm> =
        s.preferred_anti_affinity.iter().map(|w| w.pod_affinity_term.clone()).collect();
    let ns_labels = s.namespace_labels.clone();

    tally(&mut s.affinity_counts, &required_affinity, other, node, &pod.namespace, &ns_labels, sign);
    tally(
        &mut s.anti_affinity_counts,
        &required_anti,
        other,
        node,
        &pod.namespace,
        &ns_labels,
        sign,
    );
    tally(
        &mut s.preferred_affinity_counts,
        &preferred_affinity,
        other,
        node,
        &pod.namespace,
        &ns_labels,
        sign,
    );
    tally(
        &mut s.preferred_anti_affinity_counts,
        &preferred_anti,
        other,
        node,
        &pod.namespace,
        &ns_labels,
        sign,
    );

    for term in required_anti_affinity_of(other) {
        let Some(value) = node.labels.get(&term.topology_key) else {
            continue;
        };
        let scope = namespace_scope(term.namespaces.as_ref(), term.namespace_selector.as_ref());
        if namespace_in_scope(&scope, &other.namespace, &pod.namespace, &ns_labels)
            && matches_selector(term.label_selector.as_ref(), &pod.labels)
        {
            *s.existing_anti_affinity
                .entry(domain_key(&term.topology_key, value))
                .or_insert(0) += sign;
        }
    }

    state.write(NAME, s);
}

/// The count for this node's domain under `topology_key`, or `None` when the
/// node carries no such label.
fn count_in_domain(
    counts: &HashMap<DomainKey, i64>,
    topology_key: &str,
    node: &NodeInfo,
) -> Option<i64> {
    let value = node.labels.get(topology_key)?;
    Some(counts.get(&domain_key(topology_key, value)).copied().unwrap_or(0))
}

impl FilterPlugin for InterPodAffinity {
    fn filter(&self, state: &CycleState, _pod: &PodInfo, node: &NodeInfo) -> Status {
        let Some(s) = state.read::<AffinityState>(NAME) else {
            return Status::success();
        };

        for term in &s.required_affinity {
            match count_in_domain(&s.affinity_counts, &term.topology_key, node) {
                None => {
                    return Status::unschedulable(
                        NAME,
                        format!("node(s) had no {} label", term.topology_key),
                    )
                }
                Some(n) if n <= 0 => {
                    return Status::unschedulable(NAME, "node(s) didn't match pod affinity rules")
                }
                Some(_) => {}
            }
        }

        for term in &s.required_anti_affinity {
            match count_in_domain(&s.anti_affinity_counts, &term.topology_key, node) {
                None => {
                    return Status::unschedulable(
                        NAME,
                        format!("node(s) had no {} label", term.topology_key),
                    )
                }
                Some(n) if n > 0 => {
                    return Status::unschedulable(
                        NAME,
                        "node(s) didn't match pod anti-affinity rules",
                    )
                }
                Some(_) => {}
            }
        }

        // The symmetric half: a running pod's own anti-affinity forbidding us.
        for (key, count) in &s.existing_anti_affinity {
            if *count <= 0 {
                continue;
            }
            let Some((topology_key, value)) = key.split_once('=') else {
                continue;
            };
            if node.labels.get(topology_key).map(String::as_str) == Some(value) {
                return Status::unschedulable(
                    NAME,
                    "node(s) didn't satisfy existing pods' anti-affinity rules",
                );
            }
        }

        Status::success()
    }
}

impl PreScorePlugin for InterPodAffinity {
    fn pre_score(&self, state: &mut CycleState, _pod: &PodInfo, _nodes: &[&NodeInfo]) -> Status {
        match state.read::<AffinityState>(NAME) {
            Some(s) if s.has_preferred() => Status::success(),
            _ => {
                state.skip_score(NAME);
                Status::skip()
            }
        }
    }
}

impl ScorePlugin for InterPodAffinity {
    fn score(&self, state: &CycleState, _pod: &PodInfo, node: &NodeInfo) -> Result<i64, Status> {
        let Some(s) = state.read::<AffinityState>(NAME) else {
            return Ok(0);
        };

        let mut total = 0i64;
        for weighted in &s.preferred_affinity {
            let key = &weighted.pod_affinity_term.topology_key;
            if let Some(n) = count_in_domain(&s.preferred_affinity_counts, key, node) {
                if n > 0 {
                    total += weighted.weight as i64;
                }
            }
        }
        for weighted in &s.preferred_anti_affinity {
            let key = &weighted.pod_affinity_term.topology_key;
            if let Some(n) = count_in_domain(&s.preferred_anti_affinity_counts, key, node) {
                if n > 0 {
                    total -= weighted.weight as i64;
                }
            }
        }
        Ok(total)
    }

    fn normalize(&self, _state: &CycleState, _pod: &PodInfo, scores: &mut [i64]) -> Status {
        // Signed, because anti-affinity subtracts. The shared divide-by-max
        // helper assumes a non-negative range and would clamp every negative
        // score to zero, collapsing "mildly discouraged" and "strongly
        // discouraged" into the same value.
        normalize_signed(scores);
        Status::success()
    }

    fn weight(&self) -> i64 {
        2
    }
}

/// Map a signed range linearly onto `[0, MAX_NODE_SCORE]`.
///
/// All-equal scores map to the maximum: if no node is preferred over any
/// other, none should be penalised relative to the others.
fn normalize_signed(scores: &mut [i64]) {
    let max = scores.iter().copied().max().unwrap_or(0);
    let min = scores.iter().copied().min().unwrap_or(0);
    if max == min {
        for s in scores.iter_mut() {
            *s = MAX_NODE_SCORE;
        }
        return;
    }
    let span = max - min;
    for s in scores.iter_mut() {
        *s = MAX_NODE_SCORE * (*s - min) / span;
    }
}

#[cfg(test)]
#[path = "inter_pod_affinity_tests.rs"]
mod tests;
