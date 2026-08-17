//! `NodeResourcesFit` — does the pod's request fit in what the node has left,
//! and how good a fit is it. Score weight **1**.
//!
//! # The trap: filtering and scoring do not use the same numbers
//!
//! Filtering uses the pod's **real** requests. A pod that requests nothing
//! genuinely fits anywhere and must never be rejected for failing to fit a
//! request it did not make.
//!
//! Scoring uses [`PodInfo::non_zero_requests`], which substitutes **100m CPU**
//! and **200Mi memory** for anything unspecified. Without that, a Deployment
//! of request-less pods scores every node as equally free and packs all of
//! them onto whichever node wins the tiebreak.
//!
//! Two consequences worth stating because both are silent: using non-zero
//! requests for *filtering* rejects legitimate pods on a nearly-full node;
//! using real requests for *scoring* reintroduces the packing bug. The
//! constants are also widely misquoted as 1000m/128Mi — see
//! `cache::pod`'s constants, which carry the real values.
//!
//! # `PreFilterExtensions` is not optional here
//!
//! Preemption works by hypothetically removing victims from a node and asking
//! whether the preemptor then fits. That question is meaningless unless this
//! plugin can adjust its notion of what is committed, which is what
//! `add_pod`/`remove_pod` do. Without them preemption evicts pods and then
//! discovers the pod still does not fit.

use crate::cache::{NodeInfo, PodInfo, Resources, Snapshot};
use crate::events::{ActionType, ClusterEvent, EventResource};
use crate::framework::status::Status;
use crate::framework::{
    ClusterEventWithHint, CycleState, FilterPlugin, Plugin, PreFilterExtensions, PreFilterPlugin,
    PreScorePlugin, ScorePlugin, MAX_NODE_SCORE,
};

pub const NAME: &str = "NodeResourcesFit";

/// How to turn utilisation into a score.
#[derive(Clone, Debug, PartialEq)]
pub enum ScoringStrategy {
    /// Spread: emptier nodes score higher. The default.
    LeastAllocated,
    /// Bin-pack: fuller nodes score higher, so nodes can be emptied and
    /// scaled down.
    MostAllocated,
    /// A user-supplied piecewise-linear curve over utilisation, for shapes
    /// neither of the above expresses (e.g. "prefer 70% full").
    RequestedToCapacityRatio { shape: Vec<(i64, i64)> },
}

impl Default for ScoringStrategy {
    fn default() -> Self {
        ScoringStrategy::LeastAllocated
    }
}

/// Per-resource scoring weights. `{cpu: 1, memory: 1}` upstream.
#[derive(Clone, Debug)]
pub struct ResourceWeights(pub Vec<(String, i64)>);

impl Default for ResourceWeights {
    fn default() -> Self {
        ResourceWeights(vec![("cpu".to_string(), 1), ("memory".to_string(), 1)])
    }
}

/// The pod's requests, computed once per cycle. Preemption mutates the
/// `committed` delta as it dry-runs victims in and out.
#[derive(Clone, Debug, Default)]
struct FitState {
    requests: Resources,
    non_zero: Resources,
    /// Adjustment applied on top of the node's own committed total, so
    /// preemption's hypothetical removals are visible to `filter`.
    delta: Resources,
    /// Removals are tracked separately because `Resources` is unsigned by
    /// design (`sub` clamps at zero) and a signed delta would need a second
    /// representation for no benefit.
    freed: Resources,
    /// Preemption's hypothetical pod-count change, signed (unlike
    /// `delta`/`freed`, this one number is cheap to keep signed instead of
    /// needing an add/sub pair). Found via review: `filter`'s pod-count
    /// check used to read `node.pod_count()` raw, so evicting a victim
    /// could never unstick a node that was rejected for being at its pod
    /// limit specifically — only the resource checks below it saw
    /// preemption's dry-run removals.
    pod_count_delta: i64,
}

#[derive(Clone, Default)]
pub struct NodeResourcesFit {
    pub strategy: ScoringStrategy,
    pub weights: ResourceWeights,
    pub ignored_resources: std::collections::HashSet<String>,
}

impl Plugin for NodeResourcesFit {
    fn name(&self) -> &'static str {
        NAME
    }

    fn events_to_register(&self) -> Vec<ClusterEventWithHint> {
        // Capacity appears when a pod leaves, when a pod shrinks in place, or
        // when a node arrives or its allocatable grows. Notably absent:
        // UPDATE_NODE_CONDITION. Node heartbeats carry no capacity
        // information, and this is the plugin that rejects the most pods on a
        // busy cluster — subscribing to them here would undo the whole
        // event-diff design.
        vec![
            ClusterEventWithHint::always(ClusterEvent::new(
                EventResource::AssignedPod,
                ActionType::DELETE | ActionType::UPDATE_POD_SCALE_DOWN,
            )),
            ClusterEventWithHint::always(ClusterEvent::new(
                EventResource::Node,
                ActionType::ADD | ActionType::UPDATE_NODE_ALLOCATABLE,
            )),
        ]
    }
}

impl PreFilterPlugin for NodeResourcesFit {
    fn pre_filter(
        &self,
        state: &mut CycleState,
        pod: &PodInfo,
        _snapshot: &Snapshot,
    ) -> (Status, Option<Vec<String>>) {
        state.write(
            NAME,
            FitState {
                requests: pod.requests.clone(),
                non_zero: pod.non_zero_requests(),
                ..Default::default()
            },
        );
        (Status::success(), None)
    }

    fn extensions(&self) -> Option<&dyn PreFilterExtensions> {
        Some(self)
    }
}

impl PreFilterExtensions for NodeResourcesFit {
    fn add_pod(
        &self,
        state: &mut CycleState,
        _pod: &PodInfo,
        pod_to_add: &PodInfo,
        _node: &NodeInfo,
    ) -> Status {
        let mut fit = state.read::<FitState>(NAME).cloned().unwrap_or_default();
        fit.delta.add(&pod_to_add.requests);
        fit.pod_count_delta += 1;
        state.write(NAME, fit);
        Status::success()
    }

    fn remove_pod(
        &self,
        state: &mut CycleState,
        _pod: &PodInfo,
        pod_to_remove: &PodInfo,
        _node: &NodeInfo,
    ) -> Status {
        let mut fit = state.read::<FitState>(NAME).cloned().unwrap_or_default();
        fit.freed.add(&pod_to_remove.requests);
        fit.pod_count_delta -= 1;
        state.write(NAME, fit);
        Status::success()
    }
}

impl FilterPlugin for NodeResourcesFit {
    fn filter(&self, state: &CycleState, pod: &PodInfo, node: &NodeInfo) -> Status {
        let fit = state.read::<FitState>(NAME).cloned().unwrap_or_else(|| FitState {
            // Preemption's dry runs build states directly; falling back to the
            // pod's own requests keeps this honest there rather than passing
            // everything.
            requests: pod.requests.clone(),
            non_zero: pod.non_zero_requests(),
            ..Default::default()
        });

        // Pod count first: it is one comparison and it is the limit a busy
        // node hits before any resource does. Adjusted by preemption's
        // dry-run add/remove the same way the resource checks below are —
        // otherwise evicting a victim could never unstick a node rejected
        // for being at its pod-count limit specifically.
        let committed_pod_count = node.pod_count() + fit.pod_count_delta;
        if node.allocatable_pods > 0 && committed_pod_count >= node.allocatable_pods {
            return Status::unschedulable(NAME, "Too many pods");
        }

        let mut committed = node.requested.clone();
        committed.add(&fit.delta);
        committed.sub(&fit.freed);

        for name in fit.requests.names() {
            if self.ignored_resources.contains(&name) {
                continue;
            }
            let want = fit.requests.get(&name);
            let allocatable = node.allocatable.get(&name);
            let used = committed.get(&name);

            // An extended resource the node does not advertise at all is a
            // different failure from one it has run out of, and the message
            // is the only thing an operator sees.
            if allocatable == 0 {
                tracing::debug!(
                    node = %node.name, resource = %name,
                    "rejecting: node advertises no allocatable capacity for this resource at all"
                );
                return Status::unschedulable(NAME, format!("Insufficient {name}"));
            }
            if used + want > allocatable {
                // At debug rather than info: this fires on every rejected
                // pod on a busy cluster, same volume class as the
                // unschedulable log line itself. But without the actual
                // numbers, "Insufficient cpu" alone is unfalsifiable against
                // a live cluster's own `kubectl describe node` — found live
                // in CI chasing a flake where this plugin rejected a pod
                // for want of ~1000m while every other view of the cluster
                // (Allocated resources, the real pod list) showed under
                // 1200m committed against 4000m allocatable. Whether that
                // was a real transient shortfall or a cache-accounting bug
                // was unanswerable after the fact with only the reason
                // string to go on — this is what the next occurrence needs.
                tracing::debug!(
                    node = %node.name, resource = %name,
                    committed = used, requested = want, allocatable,
                    "rejecting: committed + requested exceeds allocatable"
                );
                return Status::unschedulable(NAME, format!("Insufficient {name}"));
            }
        }
        Status::success()
    }
}

impl PreScorePlugin for NodeResourcesFit {
    fn pre_score(&self, _state: &mut CycleState, _pod: &PodInfo, _nodes: &[&NodeInfo]) -> Status {
        Status::success()
    }
}

/// Utilisation of one resource on one node, as a percentage, including the
/// incoming pod.
///
/// Clamped to 100: a node can legitimately be over-committed (a pod grew, or
/// allocatable shrank), and an unclamped ratio would produce scores outside
/// `[0, 100]` that then survive normalization and distort every other plugin's
/// contribution to the weighted sum.
fn utilisation(requested: i64, allocatable: i64) -> i64 {
    if allocatable <= 0 {
        return MAX_NODE_SCORE;
    }
    ((requested * 100) / allocatable).clamp(0, MAX_NODE_SCORE)
}

impl ScorePlugin for NodeResourcesFit {
    fn score(&self, state: &CycleState, pod: &PodInfo, node: &NodeInfo) -> Result<i64, Status> {
        let non_zero = state
            .read::<FitState>(NAME)
            .map(|f| f.non_zero.clone())
            .unwrap_or_else(|| pod.non_zero_requests());

        let mut weighted_sum = 0i64;
        let mut total_weight = 0i64;

        for (name, weight) in &self.weights.0 {
            let allocatable = node.allocatable.get(name);
            if allocatable == 0 {
                continue;
            }
            // Scoring counts what the node already has PLUS this pod: the
            // question is "how good would this node be *after* placing it",
            // not "how good is it now".
            let requested = node.non_zero_requested.get(name) + non_zero.get(name);
            let util = utilisation(requested, allocatable);

            let resource_score = match &self.strategy {
                ScoringStrategy::LeastAllocated => MAX_NODE_SCORE - util,
                ScoringStrategy::MostAllocated => util,
                ScoringStrategy::RequestedToCapacityRatio { shape } => {
                    interpolate_shape(shape, util)
                }
            };
            weighted_sum += resource_score * weight;
            total_weight += weight;
        }

        if total_weight == 0 {
            return Ok(0);
        }
        Ok(weighted_sum / total_weight)
    }

    // No normalize: every strategy already produces 0..=100 per resource, and
    // the weighted mean of values in that range stays in it. Running the
    // default normalization anyway would rescale relative to the best node in
    // this particular cycle, which turns an absolute "how full is this node"
    // into a relative one and makes the plugin's contribution depend on which
    // other nodes happen to exist.

    fn weight(&self) -> i64 {
        1
    }
}

/// Piecewise-linear interpolation over a `(utilisation, score)` curve.
fn interpolate_shape(shape: &[(i64, i64)], util: i64) -> i64 {
    if shape.is_empty() {
        return 0;
    }
    let mut sorted: Vec<(i64, i64)> = shape.to_vec();
    sorted.sort_by_key(|(u, _)| *u);

    if util <= sorted[0].0 {
        return sorted[0].1;
    }
    if util >= sorted[sorted.len() - 1].0 {
        return sorted[sorted.len() - 1].1;
    }
    for w in sorted.windows(2) {
        let (u0, s0) = w[0];
        let (u1, s1) = w[1];
        if util >= u0 && util <= u1 {
            if u1 == u0 {
                return s1;
            }
            return s0 + (s1 - s0) * (util - u0) / (u1 - u0);
        }
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::framework::plugins::testutil::{node, pod, res};

    fn node_with(alloc_cpu: i64, alloc_mem: i64, used_cpu: i64, used_mem: i64) -> NodeInfo {
        let mut n = node("n");
        n.allocatable = res(alloc_cpu, alloc_mem);
        n.requested = res(used_cpu, used_mem);
        n.non_zero_requested = res(used_cpu, used_mem);
        n.allocatable_pods = 110;
        n
    }

    fn pod_wanting(milli_cpu: i64, memory: i64) -> PodInfo {
        let mut p = pod("p");
        p.requests = res(milli_cpu, memory);
        p
    }

    fn state_for(p: &PodInfo) -> CycleState {
        let mut s = CycleState::default();
        NodeResourcesFit::default().pre_filter(&mut s, p, &Snapshot::default());
        s
    }

    #[test]
    fn a_pod_that_fits_is_admitted() {
        let p = pod_wanting(500, 0);
        let n = node_with(2000, 0, 1000, 0);
        assert!(NodeResourcesFit::default().filter(&state_for(&p), &p, &n).is_success());
    }

    #[test]
    fn a_pod_that_does_not_fit_is_rejected_naming_the_resource() {
        let p = pod_wanting(1500, 0);
        let n = node_with(2000, 0, 1000, 0);

        let s = NodeResourcesFit::default().filter(&state_for(&p), &p, &n);
        assert!(!s.is_success());
        assert_eq!(s.reasons[0], "Insufficient cpu");
    }

    #[test]
    fn a_resource_rejection_is_resolvable_by_preemption() {
        // The whole point of preemption: evicting pods frees capacity.
        let p = pod_wanting(1500, 0);
        let n = node_with(2000, 0, 1000, 0);
        let s = NodeResourcesFit::default().filter(&state_for(&p), &p, &n);
        assert!(s.code.is_resolvable_by_preemption());
    }

    #[test]
    fn a_pod_requesting_nothing_fits_a_completely_full_node() {
        // Filtering uses REAL requests. Using the scoring substitution here
        // would reject a legitimate pod.
        let p = pod("p");
        let n = node_with(2000, 0, 2000, 0);
        assert!(NodeResourcesFit::default().filter(&state_for(&p), &p, &n).is_success());
    }

    #[test]
    fn fitting_exactly_is_fitting() {
        let p = pod_wanting(1000, 0);
        let n = node_with(2000, 0, 1000, 0);
        assert!(NodeResourcesFit::default().filter(&state_for(&p), &p, &n).is_success());
    }

    #[test]
    fn a_node_at_its_pod_ceiling_is_rejected_before_any_resource_is_checked() {
        let p = pod_wanting(1, 0);
        let mut n = node_with(2000, 0, 0, 0);
        n.allocatable_pods = 1;
        n.add_pod(std::sync::Arc::new(pod("existing")), 1);

        let s = NodeResourcesFit::default().filter(&state_for(&p), &p, &n);
        assert_eq!(s.reasons[0], "Too many pods");
    }

    #[test]
    fn hypothetically_removing_a_pod_makes_room_under_the_pod_ceiling_too() {
        // Same question as hypothetically_removing_a_pod_makes_room, but for
        // the pod-count check specifically — found via review: that check
        // used to read node.pod_count() raw, so it was the one thing
        // preemption's dry-run removals never reached. A node at its pod
        // ceiling could then never be preempted into, no matter how much
        // spare CPU/memory a victim's eviction would free.
        let p = pod_wanting(1, 0);
        let mut n = node_with(2000, 0, 0, 0);
        n.allocatable_pods = 1;
        n.add_pod(std::sync::Arc::new(pod("existing")), 1);
        let plugin = NodeResourcesFit::default();
        let mut state = state_for(&p);

        assert_eq!(plugin.filter(&state, &p, &n).reasons[0], "Too many pods");

        let victim = pod("existing");
        plugin.remove_pod(&mut state, &p, &victim, &n);

        assert!(
            plugin.filter(&state, &p, &n).is_success(),
            "the preemptor must fit once the victim is hypothetically gone, including past the pod-count ceiling"
        );
    }

    #[test]
    fn an_extended_resource_the_node_does_not_advertise_is_insufficient() {
        let mut p = pod("gpu-job");
        p.requests.set("nvidia.com/gpu", 1);
        let n = node_with(2000, 0, 0, 0);

        let s = NodeResourcesFit::default().filter(&state_for(&p), &p, &n);
        assert_eq!(s.reasons[0], "Insufficient nvidia.com/gpu");
    }

    #[test]
    fn an_extended_resource_the_node_has_spare_is_admitted() {
        let mut p = pod("gpu-job");
        p.requests.set("nvidia.com/gpu", 1);
        let mut n = node_with(2000, 0, 0, 0);
        n.allocatable.set("nvidia.com/gpu", 4);

        assert!(NodeResourcesFit::default().filter(&state_for(&p), &p, &n).is_success());
    }

    #[test]
    fn hypothetically_removing_a_pod_makes_room() {
        // Preemption's core question. Without PreFilterExtensions this test
        // fails and preemption evicts victims for nothing.
        let p = pod_wanting(1500, 0);
        let n = node_with(2000, 0, 1000, 0);
        let plugin = NodeResourcesFit::default();
        let mut state = state_for(&p);

        assert!(!plugin.filter(&state, &p, &n).is_success());

        let victim = pod_wanting(1000, 0);
        plugin.remove_pod(&mut state, &p, &victim, &n);

        assert!(
            plugin.filter(&state, &p, &n).is_success(),
            "the preemptor must fit once the victim is hypothetically gone"
        );
    }

    #[test]
    fn hypothetically_adding_a_pod_consumes_room() {
        let p = pod_wanting(500, 0);
        let n = node_with(2000, 0, 1000, 0);
        let plugin = NodeResourcesFit::default();
        let mut state = state_for(&p);

        assert!(plugin.filter(&state, &p, &n).is_success());

        plugin.add_pod(&mut state, &p, &pod_wanting(600, 0), &n);

        assert!(!plugin.filter(&state, &p, &n).is_success());
    }

    #[test]
    fn least_allocated_prefers_the_emptier_node() {
        let p = pod_wanting(0, 0);
        let plugin = NodeResourcesFit {
            strategy: ScoringStrategy::LeastAllocated,
            weights: ResourceWeights(vec![("cpu".to_string(), 1)]),
        };
        let state = state_for(&p);

        let empty = node_with(1000, 0, 0, 0);
        let half = node_with(1000, 0, 500, 0);

        let empty_score = plugin.score(&state, &p, &empty).unwrap();
        let half_score = plugin.score(&state, &p, &half).unwrap();
        assert!(empty_score > half_score, "{empty_score} should beat {half_score}");
    }

    #[test]
    fn most_allocated_prefers_the_fuller_node() {
        let p = pod_wanting(0, 0);
        let plugin = NodeResourcesFit {
            strategy: ScoringStrategy::MostAllocated,
            weights: ResourceWeights(vec![("cpu".to_string(), 1)]),
        };
        let state = state_for(&p);

        let empty = plugin.score(&state, &p, &node_with(1000, 0, 0, 0)).unwrap();
        let half = plugin.score(&state, &p, &node_with(1000, 0, 500, 0)).unwrap();
        assert!(half > empty);
    }

    #[test]
    fn scoring_counts_the_incoming_pod_not_just_what_is_already_there() {
        // "How good would this node be after placing it", not "how good is it
        // now" — otherwise every node scores identically for a big pod.
        let plugin = NodeResourcesFit {
            strategy: ScoringStrategy::LeastAllocated,
            weights: ResourceWeights(vec![("cpu".to_string(), 1)]),
        };
        let n = node_with(1000, 0, 0, 0);

        let small = pod_wanting(100, 0);
        let large = pod_wanting(900, 0);

        let small_score = plugin.score(&state_for(&small), &small, &n).unwrap();
        let large_score = plugin.score(&state_for(&large), &large, &n).unwrap();
        assert!(small_score > large_score);
    }

    #[test]
    fn a_request_less_pod_still_consumes_score_via_the_substitution() {
        // Otherwise a Deployment of request-less pods scores every node
        // identically and packs them all onto one.
        let plugin = NodeResourcesFit {
            strategy: ScoringStrategy::LeastAllocated,
            weights: ResourceWeights(vec![("cpu".to_string(), 1)]),
        };
        let p = pod("no-requests");
        let n = node_with(1000, 0, 0, 0);

        let score = plugin.score(&state_for(&p), &p, &n).unwrap();
        // 100m of 1000m = 10% used, so 90 under LeastAllocated.
        assert_eq!(score, 90);
    }

    #[test]
    fn an_overcommitted_node_does_not_score_outside_the_valid_range() {
        // Unclamped, this produces a negative score that survives into the
        // weighted sum and distorts every other plugin.
        let plugin = NodeResourcesFit {
            strategy: ScoringStrategy::LeastAllocated,
            weights: ResourceWeights(vec![("cpu".to_string(), 1)]),
        };
        let p = pod_wanting(0, 0);
        let n = node_with(1000, 0, 5000, 0);

        let score = plugin.score(&state_for(&p), &p, &n).unwrap();
        assert!((0..=MAX_NODE_SCORE).contains(&score), "score {score} out of range");
    }

    #[test]
    fn the_requested_to_capacity_shape_interpolates_between_its_points() {
        let shape = vec![(0, 0), (50, 100), (100, 0)];
        assert_eq!(interpolate_shape(&shape, 0), 0);
        assert_eq!(interpolate_shape(&shape, 50), 100);
        assert_eq!(interpolate_shape(&shape, 25), 50);
        assert_eq!(interpolate_shape(&shape, 100), 0);
        assert_eq!(interpolate_shape(&shape, 75), 50);
    }

    #[test]
    fn a_shape_clamps_outside_its_own_range() {
        let shape = vec![(20, 10), (80, 90)];
        assert_eq!(interpolate_shape(&shape, 0), 10);
        assert_eq!(interpolate_shape(&shape, 100), 90);
    }

    #[test]
    fn it_does_not_subscribe_to_node_heartbeats() {
        // This plugin rejects more pods than any other on a busy cluster, so
        // a heartbeat subscription here would undo the event-diff design
        // single-handedly.
        let heartbeat = ClusterEvent::new(EventResource::Node, ActionType::UPDATE_NODE_CONDITION);
        for reg in NodeResourcesFit::default().events_to_register() {
            assert!(!reg.event.matches(&heartbeat));
        }
    }

    #[test]
    fn it_wakes_when_a_pod_leaves_or_shrinks() {
        let plugin = NodeResourcesFit::default();
        let events = plugin.events_to_register();
        for happened in [
            ClusterEvent::new(EventResource::AssignedPod, ActionType::DELETE),
            ClusterEvent::new(EventResource::AssignedPod, ActionType::UPDATE_POD_SCALE_DOWN),
            ClusterEvent::new(EventResource::Node, ActionType::UPDATE_NODE_ALLOCATABLE),
        ] {
            assert!(
                events.iter().any(|e| e.event.matches(&happened)),
                "{happened:?} frees capacity and must wake a rejected pod"
            );
        }
    }
}
