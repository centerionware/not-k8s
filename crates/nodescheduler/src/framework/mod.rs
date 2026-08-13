//! The scheduling framework: extension points, and the contract every plugin
//! is held to.
//!
//! # The purity rule
//!
//! Everything from [`PreFilterPlugin`] through [`ScorePlugin`] is a **pure
//! function of the snapshot and the pod**. No I/O, no clock, no RNG, no
//! environment. That is enforced by the signatures — those traits are handed
//! `&Snapshot` and `&PodInfo` and nothing else, no client, no context — and it
//! is the same rule `nodestore`'s `apply()` obeys, for a related reason:
//! there, so replicas cannot diverge; here, so a placement decision is
//! reproducible from its inputs and every scoring plugin is unit-testable
//! without a cluster.
//!
//! The two places that genuinely need randomness — tie-breaking among
//! equally-scored nodes, and preemption's starting offset into the candidate
//! list — take it as an explicit parameter chosen *before* the cycle begins,
//! rather than reaching for a thread RNG in the middle of one. A test pins it;
//! production passes a fresh value per cycle.
//!
//! `Reserve` and `Permit` are also in the scheduling cycle, and they are
//! allowed to mutate plugin-local state — but still not to do I/O. Only
//! `PreBind`/`Bind`/`PostBind` talk to the apiserver, and they run on the
//! pod's own task so a slow one cannot stall placement for everyone else.
//!
//! # Registering events is not optional
//!
//! [`Plugin::events_to_register`] is what allows a pod this plugin rejected to
//! ever be tried again. A plugin that rejects pods and registers nothing
//! strands them until the 5-minute safety net — with no error, no log, and no
//! test failure. Every plugin's set is therefore asserted exactly in its own
//! unit tests.

pub mod status;

use crate::cache::{NodeInfo, PodInfo, Snapshot};
use crate::events::ClusterEvent;
use status::Status;
use std::collections::HashMap;

/// Per-cycle scratch space, written by PreFilter/PreScore and read by the
/// later points of the same plugin.
///
/// Keyed by plugin name and type-erased, mirroring upstream's `CycleState`.
/// It exists because PreFilter's whole purpose is to compute something once
/// per pod instead of once per node — with hundreds or thousands of nodes,
/// recomputing a pod's request totals or affinity selectors per node is the
/// difference between a scheduler that scales and one that doesn't.
#[derive(Default)]
pub struct CycleState {
    slots: HashMap<&'static str, Box<dyn std::any::Any + Send + Sync>>,
    /// Set by PreFilter/PreScore returning `Skip`; the framework consults it
    /// rather than each plugin re-deciding.
    skipped_filter: Vec<&'static str>,
    skipped_score: Vec<&'static str>,
}

impl CycleState {
    pub fn write<T: Send + Sync + 'static>(&mut self, plugin: &'static str, value: T) {
        self.slots.insert(plugin, Box::new(value));
    }

    pub fn read<T: Send + Sync + 'static>(&self, plugin: &'static str) -> Option<&T> {
        self.slots.get(plugin).and_then(|b| b.downcast_ref::<T>())
    }

    pub fn skip_filter(&mut self, plugin: &'static str) {
        self.skipped_filter.push(plugin);
    }

    pub fn filter_skipped(&self, plugin: &'static str) -> bool {
        self.skipped_filter.contains(&plugin)
    }

    pub fn skip_score(&mut self, plugin: &'static str) {
        self.skipped_score.push(plugin);
    }

    pub fn score_skipped(&self, plugin: &'static str) -> bool {
        self.skipped_score.contains(&plugin)
    }
}

/// Score bounds. Every plugin's `Score` output is normalized into this range
/// before the framework multiplies by the plugin's weight and sums.
pub const MIN_NODE_SCORE: i64 = 0;
pub const MAX_NODE_SCORE: i64 = 100;

/// What every plugin has, whatever extension points it implements.
pub trait Plugin: Send + Sync {
    fn name(&self) -> &'static str;

    /// The cluster events that could make a pod this plugin rejected
    /// schedulable. See the module header — an incomplete set here is a
    /// silent stall, which is why every implementation is asserted exactly in
    /// its own tests rather than eyeballed.
    ///
    /// The default is deliberately *empty* rather than "everything": a plugin
    /// that never rejects (a pure scorer) correctly needs no events, and a
    /// plugin that does reject is forced to think about it rather than
    /// inheriting a wildcard that quietly reintroduces the wake-everything
    /// behaviour this crate exists to avoid.
    fn events_to_register(&self) -> Vec<ClusterEventWithHint> {
        Vec::new()
    }
}

/// A subscription, optionally narrowed by a hint.
///
/// The hint answers "given this specific change, is *this specific pod* worth
/// retrying?" — so a Node label change wakes only the pods whose affinity
/// mentions that label, rather than every pod the plugin ever rejected.
pub struct ClusterEventWithHint {
    pub event: ClusterEvent,
    /// `None` means "always requeue on this event". Cheaper to write, and
    /// correct; it just wakes more pods than necessary.
    pub hint: Option<Box<dyn QueueingHintFn>>,
}

impl ClusterEventWithHint {
    pub fn always(event: ClusterEvent) -> Self {
        Self { event, hint: None }
    }
}

/// Whether a change is worth waking a particular pod for.
///
/// Errors are impossible by construction here (unlike upstream, where the fn
/// returns an error and the caller fails *open*). Same effect, one fewer way
/// to strand a pod: a hint that cannot decide returns [`QueueingHint::Queue`].
pub trait QueueingHintFn: Send + Sync {
    fn hint(&self, pod: &PodInfo, old: Option<&ChangedObject>, new: Option<&ChangedObject>)
        -> QueueingHint;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QueueingHint {
    /// This pod could not have been helped by this change. Leave it alone.
    Skip,
    /// Retry this pod.
    Queue,
}

/// The object a hint is reasoning about. Deliberately a small enum rather than
/// `dyn Any`: the set of things a hint can be handed is closed, and making it
/// closed means a hint cannot silently mis-downcast.
#[derive(Clone, Debug)]
pub enum ChangedObject {
    Node(Box<k8s_openapi::api::core::v1::Node>),
    Pod(Box<k8s_openapi::api::core::v1::Pod>),
    /// Anything a phase-4/5 plugin subscribes to. Carries the raw JSON so a
    /// plugin can read what it needs without this enum growing a variant per
    /// storage type.
    Other(serde_json::Value),
}

// ─────────────────────────────────────────────────────────────────────────
// Extension points, in execution order
// ─────────────────────────────────────────────────────────────────────────

/// Runs before a pod is admitted to the active queue at all.
///
/// A rejection here is *quiet*: no `PodScheduled=False` condition and no
/// event, because the pod is not being rejected by scheduling — it has not
/// entered scheduling. That is exactly the observable behaviour of a pod with
/// scheduling gates, and getting it wrong (writing a condition) makes gated
/// pods look broken to every dashboard.
pub trait PreEnqueuePlugin: Plugin {
    fn pre_enqueue(&self, pod: &PodInfo) -> Status;
}

/// Orders the active queue. Exactly one may be enabled, and it must be the
/// same across every profile — there is one queue, so two orderings is not a
/// thing that can exist.
pub trait QueueSortPlugin: Plugin {
    fn less(&self, a: &PodInfo, b: &PodInfo) -> bool;
}

/// Once per pod per cycle, before any node is looked at.
pub trait PreFilterPlugin: Plugin {
    /// May reject the pod outright, or return a restricted node set.
    fn pre_filter(
        &self,
        state: &mut CycleState,
        pod: &PodInfo,
        snapshot: &Snapshot,
    ) -> (Status, Option<Vec<String>>);

    /// Incremental re-evaluation for preemption's dry runs. `Some` only for
    /// plugins whose PreFilter state depends on which pods are on the node —
    /// NodeResourcesFit, InterPodAffinity, PodTopologySpread. Preemption is
    /// wrong without these: it removes victims hypothetically and must see
    /// the resulting state, not the original.
    fn extensions(&self) -> Option<&dyn PreFilterExtensions> {
        None
    }
}

pub trait PreFilterExtensions: Send + Sync {
    fn add_pod(
        &self,
        state: &mut CycleState,
        pod: &PodInfo,
        pod_to_add: &PodInfo,
        node: &NodeInfo,
    ) -> Status;

    fn remove_pod(
        &self,
        state: &mut CycleState,
        pod: &PodInfo,
        pod_to_remove: &PodInfo,
        node: &NodeInfo,
    ) -> Status;
}

/// Per node. Run in parallel across nodes, so it must be cheap and must
/// tolerate being cancelled mid-sweep once enough feasible nodes are found.
pub trait FilterPlugin: Plugin {
    fn filter(&self, state: &CycleState, pod: &PodInfo, node: &NodeInfo) -> Status;
}

/// Runs only when zero nodes were feasible. This is where preemption lives.
pub trait PostFilterPlugin: Plugin {
    fn post_filter(
        &self,
        state: &mut CycleState,
        pod: &PodInfo,
        snapshot: &Snapshot,
        node_statuses: &status::NodeToStatus,
    ) -> (Status, Option<String>);
}

/// Once per pod, before scoring. May return `Skip` to switch this plugin's
/// `Score` off for this cycle entirely.
pub trait PreScorePlugin: Plugin {
    fn pre_score(&self, state: &mut CycleState, pod: &PodInfo, nodes: &[&NodeInfo]) -> Status;
}

/// Per feasible node. Raw scores are normalized before weighting.
pub trait ScorePlugin: Plugin {
    fn score(&self, state: &CycleState, pod: &PodInfo, node: &NodeInfo) -> Result<i64, Status>;

    /// Map this plugin's raw scores onto `[MIN_NODE_SCORE, MAX_NODE_SCORE]`.
    ///
    /// Called once per cycle with this plugin's own scores for every feasible
    /// node. The default is identity — correct only for a plugin that already
    /// produces 0..=100, which most do not.
    fn normalize(&self, _state: &CycleState, _pod: &PodInfo, _scores: &mut [i64]) -> Status {
        Status::success()
    }

    /// Multiplier applied after normalization. The v1.33 defaults are
    /// TaintToleration 3, NodeAffinity/PodTopologySpread/InterPodAffinity 2,
    /// everything else 1 — note this differs from the published docs table,
    /// which is stale (see docs/SCHEDULER.md).
    fn weight(&self) -> i64 {
        1
    }
}

/// Claims stateful resources for the chosen node. In the scheduling cycle, so
/// it may assume no other pod is mid-cycle — but still no I/O.
pub trait ReservePlugin: Plugin {
    fn reserve(&self, state: &mut CycleState, pod: &PodInfo, node: &str) -> Status;

    /// Must not fail and must be idempotent: it runs on every failure at or
    /// after Reserve, in reverse plugin order, and there is nothing sensible
    /// to do if the cleanup itself errors.
    fn unreserve(&self, state: &mut CycleState, pod: &PodInfo, node: &str);
}

/// Can hold a pod in the binding cycle (gang scheduling). No default plugin
/// implements this; it exists because the extension point is part of the
/// contract third-party plugins are written against.
pub trait PermitPlugin: Plugin {
    fn permit(&self, state: &mut CycleState, pod: &PodInfo, node: &str)
        -> (Status, Option<std::time::Duration>);
}

/// Side effects that must land before the Binding object exists. The binding
/// cycle starts here — everything from this point is async and per-pod, which
/// is what keeps VolumeBinding's up-to-600s wait off the scheduling loop.
#[async_trait::async_trait]
pub trait PreBindPlugin: Plugin {
    async fn pre_bind(&self, pod: &PodInfo, node: &str) -> Status;
}

/// Writes the Binding. The first plugin that does not return `Skip` wins.
#[async_trait::async_trait]
pub trait BindPlugin: Plugin {
    async fn bind(&self, pod: &PodInfo, node: &str) -> Status;
}

/// Informational only; cannot fail the binding.
#[async_trait::async_trait]
pub trait PostBindPlugin: Plugin {
    async fn post_bind(&self, pod: &PodInfo, node: &str);
}

/// Everything enabled for one profile, in execution order.
///
/// A profile is selected by a pod's `spec.schedulerName`. Multiple profiles
/// share one queue, which is why they must share a QueueSort plugin.
#[derive(Default)]
pub struct Registry {
    pub profile_name: String,
    pub pre_enqueue: Vec<Box<dyn PreEnqueuePlugin>>,
    pub queue_sort: Option<Box<dyn QueueSortPlugin>>,
    pub pre_filter: Vec<Box<dyn PreFilterPlugin>>,
    pub filter: Vec<Box<dyn FilterPlugin>>,
    pub post_filter: Vec<Box<dyn PostFilterPlugin>>,
    pub pre_score: Vec<Box<dyn PreScorePlugin>>,
    pub score: Vec<Box<dyn ScorePlugin>>,
    pub reserve: Vec<Box<dyn ReservePlugin>>,
    pub permit: Vec<Box<dyn PermitPlugin>>,
    pub pre_bind: Vec<Box<dyn PreBindPlugin>>,
    pub bind: Vec<Box<dyn BindPlugin>>,
    pub post_bind: Vec<Box<dyn PostBindPlugin>>,
}

impl Registry {
    /// Every distinct cluster event any enabled plugin subscribes to.
    ///
    /// This is also the informer list: upstream starts Pod and Node watches
    /// unconditionally and every other watch only because some enabled plugin
    /// named that resource. Copying that is both the parity behaviour and the
    /// footprint behaviour — on a cluster with no PVs, `--scheduler=
    /// nodescheduler` costs two watches rather than nine.
    pub fn subscribed_resources(&self) -> Vec<crate::events::EventResource> {
        let mut out: Vec<crate::events::EventResource> = Vec::new();
        let mut push = |evs: Vec<ClusterEventWithHint>| {
            for e in evs {
                if !out.contains(&e.event.resource) {
                    out.push(e.event.resource);
                }
            }
        };
        for p in &self.pre_enqueue {
            push(p.events_to_register());
        }
        for p in &self.pre_filter {
            push(p.events_to_register());
        }
        for p in &self.filter {
            push(p.events_to_register());
        }
        for p in &self.post_filter {
            push(p.events_to_register());
        }
        for p in &self.reserve {
            push(p.events_to_register());
        }
        out.sort_unstable();
        out.dedup();
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cycle_state_round_trips_per_plugin() {
        let mut s = CycleState::default();
        s.write("NodeResourcesFit", 42u32);
        s.write("NodePorts", "hello".to_string());

        assert_eq!(s.read::<u32>("NodeResourcesFit"), Some(&42));
        assert_eq!(s.read::<String>("NodePorts"), Some(&"hello".to_string()));
        assert_eq!(s.read::<u32>("NodePorts"), None, "wrong type must miss, not panic");
        assert_eq!(s.read::<u32>("Absent"), None);
    }

    #[test]
    fn skips_are_tracked_per_extension_point() {
        let mut s = CycleState::default();
        s.skip_filter("VolumeBinding");
        s.skip_score("InterPodAffinity");

        assert!(s.filter_skipped("VolumeBinding"));
        assert!(!s.score_skipped("VolumeBinding"));
        assert!(s.score_skipped("InterPodAffinity"));
        assert!(!s.filter_skipped("InterPodAffinity"));
    }
}
