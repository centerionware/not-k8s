//! `DefaultPreemption` — make room for a pod by evicting less important ones.
//!
//! Runs as `PostFilter`, and only when **zero** nodes were feasible. It is the
//! most destructive thing this component can do, so nearly every rule here is
//! about doing as little of it as possible.
//!
//! # The shape
//!
//! ```text
//! eligible?          -> may this pod preempt at all
//! candidate nodes    -> only those rejected with Unschedulable, never
//!                       UnschedulableAndUnresolvable — eviction cannot fix
//!                       a wrong node name or an unmatched affinity
//! per node: victims  -> remove every lower-priority pod, check the preemptor
//!                       fits, then put back everything that was not needed
//! pick one node      -> six tiebreaks, in order
//! ```
//!
//! # Reprieve is the part that makes it minimal
//!
//! Removing all lower-priority pods and declaring them victims would work and
//! would be catastrophic — a big pod would empty a node. Instead every removed
//! pod is offered back one at a time, cheapest-to-lose last, and kept if the
//! preemptor still fits without it. Only pods whose absence is genuinely
//! required end up as victims.
//!
//! The order of that offering decides who dies. Pods whose removal would
//! breach a `PodDisruptionBudget` are offered back **first**, so they are the
//! most likely to be spared — which is the whole point of a PDB. Inverting
//! this still produces a working preemption that evicts a different, worse set
//! of pods, and no test of "did the preemptor get scheduled" would notice.
//!
//! # PDB accounting has an exemption that is easy to miss
//!
//! A pod already listed in a PDB's `status.disruptedPods` has *already* been
//! counted against that budget by whoever is disrupting it. Counting it again
//! double-charges the budget and makes preemption believe it has less headroom
//! than it does, so it spares pods it could legitimately take.

use crate::cache::{NodeInfo, PodInfo};
use crate::cycle::Rng;
use k8s_openapi::api::policy::v1::PodDisruptionBudget;
use std::collections::HashMap;

/// `minCandidateNodesPercentage`, upstream's default.
pub const MIN_CANDIDATE_NODES_PERCENTAGE: i32 = 10;
/// `minCandidateNodesAbsolute`, upstream's default.
pub const MIN_CANDIDATE_NODES_ABSOLUTE: i32 = 100;

/// Why a pod may not preempt.
#[derive(Debug, PartialEq, Eq)]
pub enum Ineligible {
    /// `preemptionPolicy: Never`.
    PolicyNever,
    /// A previous preemption for this pod is still draining. Starting another
    /// would evict a second set of pods for room that is already being made.
    NominationInProgress,
}

/// May this pod preempt others?
///
/// `nominated_node_draining` is true when the pod already holds a
/// `nominatedNodeName` **and** that node still has a lower-priority pod with a
/// deletion timestamp — i.e. the eviction it asked for last time has not
/// finished. `nominated_node_unresolvable` overrides that: if the nomination
/// is known dead, holding the pod back would strand it forever.
pub fn eligible_to_preempt(
    preemption_policy: Option<&str>,
    nominated_node_draining: bool,
    nominated_node_unresolvable: bool,
) -> Result<(), Ineligible> {
    if preemption_policy == Some("Never") {
        return Err(Ineligible::PolicyNever);
    }
    if nominated_node_draining && !nominated_node_unresolvable {
        return Err(Ineligible::NominationInProgress);
    }
    Ok(())
}

/// Where to start scanning, and how many candidates are enough.
///
/// The random offset matters more than it looks: without it preemption always
/// evaluates the same prefix of nodes, so the same unlucky nodes are chewed
/// over and over while the rest of the cluster is never considered.
pub fn offset_and_num_candidates(num_potential: i32, rng: &mut Rng) -> (i32, i32) {
    if num_potential <= 0 {
        return (0, 0);
    }
    let offset = rng.below(num_potential as u64) as i32;
    let by_percentage = num_potential * MIN_CANDIDATE_NODES_PERCENTAGE / 100;
    let num = by_percentage.max(MIN_CANDIDATE_NODES_ABSOLUTE).min(num_potential);
    (offset, num)
}

/// Upstream's `MoreImportantPod`: higher priority first, and among equals the
/// **longer-running** first.
///
/// The tiebreak is deliberate and is not arbitrary fairness. A pod that has
/// been running for hours has more accumulated state and more to lose from
/// being killed than one that started a minute ago, so among equally important
/// pods the young one is taken first. Sorting the other way evicts exactly the
/// wrong member of a ReplicaSet.
pub fn more_important(a: &PodInfo, b: &PodInfo) -> std::cmp::Ordering {
    b.priority
        .cmp(&a.priority)
        .then_with(|| a.queued_at.cmp(&b.queued_at))
}

/// Whether removing this pod would breach a PodDisruptionBudget.
///
/// `disruptions_allowed` is decremented per matched victim as the caller walks
/// them, so this is called in sequence and mutates the running budget.
pub fn violates_pdb(
    pod: &PodInfo,
    budgets: &mut [PdbState],
) -> bool {
    let mut violating = false;
    for pdb in budgets.iter_mut() {
        if !pdb.matches(pod) {
            continue;
        }
        // Already booked by whoever is disrupting it — charging the budget a
        // second time makes preemption believe it has less headroom than it
        // really does. See the module header.
        if pdb.already_disrupted.contains(&pod.name) {
            continue;
        }
        if pdb.disruptions_allowed <= 0 {
            violating = true;
        } else {
            pdb.disruptions_allowed -= 1;
        }
    }
    violating
}

/// A PodDisruptionBudget, projected to what preemption needs.
#[derive(Clone, Debug)]
pub struct PdbState {
    pub namespace: String,
    pub selector: Option<k8s_openapi::apimachinery::pkg::apis::meta::v1::LabelSelector>,
    pub disruptions_allowed: i32,
    /// `status.disruptedPods` keys — pods whose disruption is already booked.
    pub already_disrupted: Vec<String>,
}

impl PdbState {
    pub fn matches(&self, pod: &PodInfo) -> bool {
        pod.namespace == self.namespace
            && crate::framework::plugins::selector::matches_selector(
                self.selector.as_ref(),
                &pod.labels,
            )
    }

    /// Project the API object.
    pub fn from_api(pdb: &PodDisruptionBudget) -> Self {
        let status = pdb.status.clone();
        PdbState {
            namespace: pdb.metadata.namespace.clone().unwrap_or_default(),
            selector: pdb.spec.as_ref().and_then(|s| s.selector.clone()),
            disruptions_allowed: status.as_ref().map(|s| s.disruptions_allowed).unwrap_or(0),
            already_disrupted: status
                .as_ref()
                .and_then(|s| s.disrupted_pods.as_ref())
                .map(|m| m.keys().cloned().collect())
                .unwrap_or_default(),
        }
    }
}

/// What preemption found on one node.
#[derive(Debug, Default, Clone)]
pub struct Victims {
    pub pods: Vec<String>,
    /// How many of them breach a PDB. A tiebreak, and a thing to report.
    pub pdb_violations: usize,
}

/// The minimal set of pods whose removal lets `preemptor` fit on `node`.
///
/// `fits` is the caller's feasibility check against a hypothetical pod set —
/// in production, re-running the Filter plugins with the removed pods taken
/// out of `CycleState`; in tests, anything.
///
/// Returns `None` when the preemptor does not fit even with **every**
/// lower-priority pod gone, which means this node is not a candidate at all
/// and nothing should be evicted on it.
pub fn select_victims_on_node<F>(
    preemptor: &PodInfo,
    node: &NodeInfo,
    budgets: &mut [PdbState],
    mut fits: F,
) -> Option<Victims>
where
    F: FnMut(&[&PodInfo]) -> bool,
{
    // Only pods strictly less important may be taken. Equal priority is not
    // enough — preemption is for making room for something *more* important,
    // and allowing equals would let same-priority pods evict each other in a
    // loop.
    let mut potential: Vec<&PodInfo> = node
        .pods
        .iter()
        .filter(|p| p.priority < preemptor.priority)
        .map(|p| p.as_ref())
        .collect();

    // Partition by PDB, then order each part by importance.
    potential.sort_by(|a, b| more_important(a, b));

    let mut violating: Vec<&PodInfo> = Vec::new();
    let mut non_violating: Vec<&PodInfo> = Vec::new();
    for p in &potential {
        if violates_pdb(p, budgets) {
            violating.push(p);
        } else {
            non_violating.push(p);
        }
    }

    // The gate: with every removable pod gone, does the preemptor fit? If not,
    // this node cannot be made to work and *nothing on it should die* — the
    // check that stops preemption evicting pods for a pod it still cannot
    // place. A node where the preemptor already fits needs no victims and
    // falls out of the reprieve loop below with an empty list.
    let all_removed: Vec<&PodInfo> = potential.clone();
    if !fits(&all_removed) {
        return None;
    }

    // Now put pods back, keeping any whose absence turns out not to be
    // needed. PDB-violating pods are offered back FIRST, so they are the most
    // likely to be spared — see the module header.
    let mut still_removed: Vec<&PodInfo> = potential.clone();
    let mut victims: Vec<&PodInfo> = Vec::new();
    let mut pdb_violations = 0usize;

    // Both groups are already sorted most-important-first, and the offering
    // runs in exactly that order: whoever is offered back first is most
    // likely to be spared, because the preemptor still has room at that
    // point. PDB-covered pods lead, then the important, then the cheap — so
    // the pods that stay evicted are the least important non-protected ones.
    //
    // Iterating this list in reverse inverts the whole rule: the cheapest
    // pods get spared and the important ones die, while the preemptor is
    // still placed successfully and every outcome-based test still passes.
    // That is precisely what the first version did, and what
    // `the_least_important_pod_is_taken_first` caught.
    let offer_order: Vec<(&PodInfo, bool)> = violating
        .iter()
        .map(|p| (*p, true))
        .chain(non_violating.iter().map(|p| (*p, false)))
        .collect();

    for (candidate, is_violating) in offer_order {
        // Try putting this one back.
        let trial: Vec<&PodInfo> = still_removed
            .iter()
            .copied()
            .filter(|p| p.uid != candidate.uid)
            .collect();
        if fits(&trial) {
            // Not needed after all — reprieved.
            still_removed = trial;
        } else {
            victims.push(candidate);
            if is_violating {
                pdb_violations += 1;
            }
        }
    }

    Some(Victims {
        // Namespaced key, not bare name — two pods on the same node can
        // legitimately share a name across namespaces, and a bare name
        // would let both callers in cycle.rs match the wrong one when they
        // filter `node.pods` back down to this set.
        pods: victims.iter().map(|p| p.key()).collect(),
        pdb_violations,
    })
}

/// One node's candidacy, for the final choice.
#[derive(Debug, Clone)]
pub struct Candidate {
    pub node: String,
    pub victims: Victims,
    /// Highest priority among the victims — the worst thing this choice kills.
    pub highest_victim_priority: i32,
    pub sum_victim_priorities: i64,
    /// Latest start among the highest-priority victims. Later is better: it
    /// means the most important thing being killed is at least the youngest
    /// of its kind.
    pub latest_start_of_highest: Option<k8s_openapi::jiff::Timestamp>,
}

/// Upstream's `pickOneNodeForPreemption`: six tiebreaks, in order.
///
/// Each stage narrows the set; the first node survives an exact tie. The
/// ordering encodes "do the least harm" three different ways — fewest broken
/// budgets, then least important casualty, then least total damage — before
/// falling back to simple counts.
pub fn pick_one_node(candidates: &[Candidate]) -> Option<&Candidate> {
    if candidates.is_empty() {
        return None;
    }

    fn narrow<'a>(set: &mut Vec<&'a Candidate>, key: impl Fn(&Candidate) -> i64) {
        if set.len() <= 1 {
            return;
        }
        let best = set.iter().map(|c| key(c)).min().unwrap_or(0);
        set.retain(|c| key(c) == best);
    }

    let mut set: Vec<&Candidate> = candidates.iter().collect();

    // Every stage is "smallest wins", so a preference for *larger* is encoded
    // by negating rather than by a second comparator — one direction to get
    // right instead of six.
    narrow(&mut set, |c| c.victims.pdb_violations as i64);
    narrow(&mut set, |c| c.highest_victim_priority as i64);
    narrow(&mut set, |c| c.sum_victim_priorities);
    narrow(&mut set, |c| c.victims.pods.len() as i64);
    // Latest start among the highest-priority victims: kill the youngest of
    // the worst.
    narrow(&mut set, |c| -c.latest_start_of_highest.map(|t| t.as_second()).unwrap_or(0));

    // 6. First in list order — an exact tie is resolved by whatever the
    //    caller's node ordering already decided, which is the zone-round-robin
    //    from the snapshot.
    set.into_iter().next()
}

/// Tracks which pods have been promised which node.
///
/// A pod that has preempted holds a `nominatedNodeName` while its victims
/// drain. Two things depend on it, and the second is the one that is easy to
/// omit:
///
///   1. the pod is retried against that node first;
///   2. **other pods filtering that node must see the nominee as already
///      there.** Without it two preemptors both see the freed capacity, both
///      claim it, and one of them is wrong — a load-dependent double-booking
///      that no single-pod test reproduces.
#[derive(Debug, Default)]
pub struct Nominator {
    by_pod: HashMap<String, String>,
    /// The nominees themselves, not just their ids: a filter injecting them
    /// needs their requests, labels and affinity terms, and re-deriving that
    /// from the snapshot is impossible because a nominated pod is by
    /// definition not placed yet.
    pods: HashMap<String, std::sync::Arc<PodInfo>>,
    by_node: HashMap<String, Vec<String>>,
}

impl Nominator {
    pub fn nominate(&mut self, pod: std::sync::Arc<PodInfo>, node: &str) {
        let uid = pod.uid.clone();
        self.remove(&uid);
        self.by_pod.insert(uid.clone(), node.to_string());
        self.pods.insert(uid.clone(), pod);
        self.by_node.entry(node.to_string()).or_default().push(uid);
    }

    pub fn remove(&mut self, pod_uid: &str) {
        self.pods.remove(pod_uid);
        if let Some(node) = self.by_pod.remove(pod_uid) {
            if let Some(list) = self.by_node.get_mut(&node) {
                list.retain(|u| u != pod_uid);
                if list.is_empty() {
                    self.by_node.remove(&node);
                }
            }
        }
    }

    pub fn nominated_node(&self, pod_uid: &str) -> Option<&str> {
        self.by_pod.get(pod_uid).map(String::as_str)
    }

    /// Pods promised this node, which a filter must treat as already present.
    pub fn nominated_on(&self, node: &str) -> Vec<std::sync::Arc<PodInfo>> {
        self.by_node
            .get(node)
            .map(|uids| uids.iter().filter_map(|u| self.pods.get(u).cloned()).collect())
            .unwrap_or_default()
    }

    pub fn len(&self) -> usize {
        self.by_pod.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_pod.is_empty()
    }
}

#[cfg(test)]
#[path = "preempt_tests.rs"]
mod tests;
