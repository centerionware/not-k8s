//! The default plugin set, one file per plugin.
//!
//! # What "default" means here, and why it isn't copied from the docs
//!
//! [`default_registry`] builds the **v1.33 default profile**, taken from
//! `pkg/scheduler/apis/config/v1/default_plugins.go` in `release-1.33` rather
//! than from the published reference table, which is stale in two ways that
//! change behaviour (see docs/SCHEDULER.md, "Where the docs are wrong"): it
//! still lists the removed in-tree cloud volume-limit plugins, and its score
//! weights are all 1 when the real ones are TaintToleration 3 and
//! NodeAffinity/PodTopologySpread/InterPodAffinity 2. A weight table that is
//! wrong does not fail anything — it just places pods slightly differently
//! than upstream would, forever, which is precisely the class of divergence
//! this project exists not to have.
//!
//! Phase 1 is everything that needs no informer beyond Pod and Node; later
//! phases (topology, preemption, storage, DRA) added their own plugins and
//! informers on top without touching Phase 1's shape. Multi-profile dispatch
//! (`lib.rs`'s `schedule_forever` calls this once per
//! `NODESCHEDULER_PROFILE_NAME` entry) means the `Registry` this builds is
//! not necessarily *the* profile — see docs/SCHEDULER.md's "Phase 5".
//!
//! # Two rules every file in this directory obeys
//!
//! 1. **Pure.** Everything except [`default_binder`] is a function of the
//!    snapshot and the pod: no I/O, no clock, no RNG, no environment. The
//!    trait signatures enforce it; the point of the rule is that every
//!    scoring formula below is testable against a hand-computed number
//!    without a cluster, the same way `nodeproxy`'s `build_ruleset()` is.
//! 2. **A plugin that can reject registers the events that could un-reject
//!    it.** Missing one is not an error and not a log line — it is a pod that
//!    sits still until the 5-minute safety net notices. Every plugin here
//!    therefore has a test asserting its registered set *exactly*, so adding
//!    a new rejection reason without adding its wake-up event fails the build
//!    instead of the cluster.
//!
//! # Assumptions about `crate::cache`
//!
//! The projection types (`NodeInfo`, `PodInfo`, `Resources`) are consumed
//! here through a deliberately small surface, listed in one place so a rename
//! upstream of this module is a compile error in a handful of spots rather
//! than a rewrite:
//!
//! * `NodeInfo`: `name`, `labels`, `taints`, `allocatable`, `requested`,
//!   `non_zero_requested`, `allocatable_pods`, `used_ports`, `images`,
//!   `unschedulable`, `pods`.
//! * `PodInfo`: `namespace`, `name`, `uid`, `priority`, `queued_at`,
//!   `node_selector`, `node_affinity`, `tolerations`, `scheduling_gates`,
//!   `host_ports`, `images`, `requests`, `node_name`.
//! * `Resources`: `milli_cpu`, `memory`, `ephemeral_storage`, plus the
//!   `hugepages` and `extended` maps of resource-name → quantity.

pub mod default_binder;
pub mod dynamic_resources;
pub mod image_locality;
pub mod inter_pod_affinity;
pub mod node_affinity;
pub mod node_name;
pub mod node_ports;
pub mod node_resources_balanced_allocation;
pub mod node_resources_fit;
pub mod node_unschedulable;
pub mod node_volume_limits;
pub mod pod_topology_spread;
pub mod priority_sort;
pub mod quantity;
pub mod scheduling_gates;
pub mod selector;
pub mod taint_toleration;
pub mod volume_binding;
pub mod volume_restrictions;
pub mod volume_zone;

use super::{MAX_NODE_SCORE, Registry};

/// Upstream's `helper.DefaultNormalizeScore`, which several plugins share.
///
/// Maps raw scores onto `[0, MAX_NODE_SCORE]` by dividing through by the
/// largest score present. `reverse` flips the result, for plugins whose raw
/// score counts something bad (TaintToleration counts un-tolerated
/// `PreferNoSchedule` taints, so more is worse).
///
/// The degenerate case is the one worth stating: when every raw score is 0
/// there is nothing to divide by, and the answer is *not* "leave them at 0"
/// for a reversed plugin — zero un-tolerated taints everywhere means every
/// node is perfect, so they all get the maximum. Getting that branch wrong
/// makes a cluster with no taints at all score uniformly 0 for
/// TaintToleration, which is invisible in isolation and quietly changes the
/// balance between plugins.
pub(crate) fn default_normalize_score(reverse: bool, scores: &mut [i64]) {
    let max = scores.iter().copied().max().unwrap_or(0);
    if max <= 0 {
        if reverse {
            for s in scores.iter_mut() {
                *s = MAX_NODE_SCORE;
            }
        }
        return;
    }
    for s in scores.iter_mut() {
        let scaled = MAX_NODE_SCORE * *s / max;
        *s = if reverse { MAX_NODE_SCORE - scaled } else { scaled };
    }
}

/// The v1.33 default profile, in upstream's MultiPoint order.
///
/// Order matters at every extension point for the same reason it does
/// upstream: the first rejecting Filter short-circuits the rest for that
/// node, so the cheap unconditional checks (`NodeUnschedulable`, `NodeName`)
/// run before the ones that touch resource arithmetic. It is *not* a
/// correctness property — the set of feasible nodes is the same in any order
/// — but it is a measurable one on a large cluster, and matching upstream
/// means a profile dumped from either scheduler compares equal.
///
/// `client` is used by [`default_binder::DefaultBinder`] (writes the
/// Binding), [`volume_binding::VolumeBinding`] (annotates and polls a PVC in
/// `PreBind`), and [`dynamic_resources::DynamicResources`] (writes a
/// ResourceClaim's allocation and reservation in `PreBind`) — the only three
/// plugins in this directory that perform I/O; everything else here is a
/// pure function of the snapshot.
pub fn default_registry(client: kube::Client, cfg: &crate::config::Config, profile_name: &str) -> Registry {
    // One instance each, cloned into every vec it belongs to — see
    // `DynamicResources`'s (respectively `VolumeBinding`'s) own doc comment
    // on why each plugin's assume cache must be shared rather than
    // reconstructed per extension point.
    let dra = dynamic_resources::DynamicResources::new(client.clone());
    let vb = volume_binding::VolumeBinding::new(client.clone(), cfg.volume_bind_timeout);
    Registry {
        profile_name: profile_name.to_string(),
        pre_enqueue: vec![
            Box::new(scheduling_gates::SchedulingGates),
            Box::new(dra.clone()),
        ],
        queue_sort: Some(Box::new(priority_sort::PrioritySort)),
        pre_filter: vec![
            Box::new(node_ports::NodePorts),
            Box::new(node_resources_fit::NodeResourcesFit::default()),
            Box::new(volume_restrictions::VolumeRestrictions),
            Box::new(node_volume_limits::NodeVolumeLimits),
            Box::new(vb.clone()),
            Box::new(volume_zone::VolumeZone),
            Box::new(pod_topology_spread::PodTopologySpread {
                defaulting: cfg.topology_defaulting,
            }),
            Box::new(inter_pod_affinity::InterPodAffinity::default()),
            Box::new(dra.clone()),
        ],
        filter: vec![
            Box::new(node_unschedulable::NodeUnschedulable),
            Box::new(node_name::NodeName),
            Box::new(taint_toleration::TaintToleration),
            Box::new(node_affinity::NodeAffinity::default()),
            Box::new(node_ports::NodePorts),
            Box::new(node_resources_fit::NodeResourcesFit::default()),
            Box::new(volume_restrictions::VolumeRestrictions),
            Box::new(node_volume_limits::NodeVolumeLimits),
            Box::new(vb.clone()),
            Box::new(volume_zone::VolumeZone),
            Box::new(pod_topology_spread::PodTopologySpread {
                defaulting: cfg.topology_defaulting,
            }),
            Box::new(inter_pod_affinity::InterPodAffinity::default()),
            Box::new(dra.clone()),
        ],
        // Runs before `Scheduler::preempt`'s fallback (see
        // `PostFilterPlugin`'s own doc comment): freeing a claim this pod
        // already owns but can't use anywhere is strictly cheaper than
        // evicting another pod, and upstream orders its own registered
        // plugins the same way.
        post_filter: vec![Box::new(dra.clone())],
        pre_score: vec![
            Box::new(taint_toleration::TaintToleration),
            Box::new(node_affinity::NodeAffinity::default()),
            Box::new(image_locality::ImageLocality),
            Box::new(node_resources_fit::NodeResourcesFit::default()),
            Box::new(
                node_resources_balanced_allocation::NodeResourcesBalancedAllocation::default(),
            ),
            Box::new(pod_topology_spread::PodTopologySpread {
                defaulting: cfg.topology_defaulting,
            }),
            Box::new(inter_pod_affinity::InterPodAffinity::default()),
        ],
        score: vec![
            Box::new(taint_toleration::TaintToleration),
            Box::new(node_affinity::NodeAffinity::default()),
            Box::new(image_locality::ImageLocality),
            Box::new(node_resources_fit::NodeResourcesFit::default()),
            Box::new(
                node_resources_balanced_allocation::NodeResourcesBalancedAllocation::default(),
            ),
            Box::new(pod_topology_spread::PodTopologySpread {
                defaulting: cfg.topology_defaulting,
            }),
            Box::new(inter_pod_affinity::InterPodAffinity::default()),
        ],
        reserve: vec![Box::new(vb.clone()), Box::new(dra.clone())],
        permit: Vec::new(),
        pre_bind: vec![Box::new(vb.clone()), Box::new(dra.clone())],
        bind: vec![Box::new(default_binder::DefaultBinder::new(client))],
        post_bind: vec![Box::new(vb), Box::new(dra)],
    }
}

/// Fixtures shared by every plugin's tests.
///
/// Deliberately one place: these build `cache` projections field by field,
/// so a change to those types costs one edit here rather than one per test.
#[cfg(test)]
pub(crate) mod testutil {
    use crate::cache::{NodeInfo, PodInfo, Resources};

    pub fn node(name: &str) -> NodeInfo {
        NodeInfo { name: name.to_string(), ..Default::default() }
    }

    pub fn pod(name: &str) -> PodInfo {
        PodInfo { name: name.to_string(), ..Default::default() }
    }

    /// cpu in millicores, memory in bytes — the units the projection stores.
    pub fn res(milli_cpu: i64, memory: i64) -> Resources {
        Resources { milli_cpu, memory, ..Default::default() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizing_scales_the_largest_score_to_the_maximum() {
        let mut scores = [0, 5, 10];
        default_normalize_score(false, &mut scores);
        assert_eq!(scores, [0, 50, 100]);
    }

    #[test]
    fn a_reversed_normalization_makes_the_largest_raw_score_the_worst_node() {
        let mut scores = [0, 5, 10];
        default_normalize_score(true, &mut scores);
        assert_eq!(scores, [100, 50, 0]);
    }

    #[test]
    fn an_all_zero_reversed_normalization_gives_every_node_the_maximum() {
        // Zero un-tolerated taints anywhere means every node is equally
        // perfect, not equally bad. The natural "divide by max" implementation
        // leaves these at 0 and silently de-weights the plugin.
        let mut scores = [0, 0, 0];
        default_normalize_score(true, &mut scores);
        assert_eq!(scores, [100, 100, 100]);
    }

    #[test]
    fn an_all_zero_forward_normalization_leaves_every_node_at_zero() {
        let mut scores = [0, 0, 0];
        default_normalize_score(false, &mut scores);
        assert_eq!(scores, [0, 0, 0]);
    }

    #[test]
    fn a_single_node_normalizes_to_the_extreme_of_its_own_range() {
        let mut one = [7];
        default_normalize_score(false, &mut one);
        assert_eq!(one, [100]);

        let mut one_reversed = [7];
        default_normalize_score(true, &mut one_reversed);
        assert_eq!(one_reversed, [0]);
    }

    #[test]
    fn equal_nonzero_scores_all_normalize_to_the_same_value() {
        let mut scores = [4, 4, 4];
        default_normalize_score(false, &mut scores);
        assert_eq!(scores, [100, 100, 100]);
    }

    #[test]
    fn normalizing_an_empty_score_list_does_not_panic() {
        let mut empty: [i64; 0] = [];
        default_normalize_score(true, &mut empty);
        assert!(empty.is_empty());
    }
}
