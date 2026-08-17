//! The event vocabulary — and the old/new diff that produces it.
//!
//! # This file is where the footprint claim lives
//!
//! A scheduler's idle cost is watch-event decoding, not timers (see
//! docs/SCHEDULER.md for that accounting). The highest-frequency event in any
//! real cluster is the kubelet node-status heartbeat: one Node update per node
//! per `node-monitor-period`, which this project's control plane sets to 10s.
//! On a 200-node cluster that is 20 Node updates a second, forever, at
//! complete idle.
//!
//! Almost all of them change nothing a scheduler could care about — only
//! `status.conditions[*].lastHeartbeatTime` moves. So the job of
//! [`node_action_types`] is to look at old and new and return **no bits at
//! all** for that case, which means no plugin's registered event matches,
//! which means no pod in `unschedulablePods` is even considered for requeue.
//!
//! Emitting a generic "Update" for every Node event would be correct and
//! would also reintroduce exactly the cost this component exists to remove,
//! at a rate proportional to cluster size. That is the trap this module is
//! written to avoid, and it is why the diff functions are pure and unit
//! tested against hand-built pairs rather than being inlined into `watch.rs`.

use k8s_openapi::api::core::v1::{Node, Pod};
use std::collections::BTreeMap;

/// The resources a plugin can subscribe to.
///
/// Mirrors upstream's `EventResource`. `AssignedPod` and `UnschedulablePod`
/// are deliberately distinct from `Pod`: a pod that already has a node
/// affects the *cache* (its resources are consumed), while a pod that does
/// not affects the *queue* (it needs scheduling). Conflating them was a real
/// upstream bug class — assigned-pod churn must never enqueue anything
/// except through an event.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum EventResource {
    Pod,
    AssignedPod,
    UnschedulablePod,
    Node,
    Namespace,
    PersistentVolume,
    PersistentVolumeClaim,
    CsiNode,
    CsiDriver,
    CsiStorageCapacity,
    VolumeAttachment,
    StorageClass,
    ResourceClaim,
    ResourceSlice,
    DeviceClass,
    /// Matches every resource. Used by the unschedulable-timeout safety net
    /// and by forced activation.
    WildCard,
}

/// What happened to a resource, as a bitmask.
///
/// The fine-grained `UPDATE_*` bits are the whole point — see this module's
/// header. A plugin subscribes to the specific bits it can be un-stuck by,
/// and an event wakes it only if the intersection is non-empty.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct ActionType(pub u32);

impl ActionType {
    pub const NONE: ActionType = ActionType(0);
    pub const ADD: ActionType = ActionType(1 << 0);
    pub const DELETE: ActionType = ActionType(1 << 1);
    pub const UPDATE_NODE_ALLOCATABLE: ActionType = ActionType(1 << 2);
    pub const UPDATE_NODE_LABEL: ActionType = ActionType(1 << 3);
    pub const UPDATE_NODE_TAINT: ActionType = ActionType(1 << 4);
    pub const UPDATE_NODE_CONDITION: ActionType = ActionType(1 << 5);
    pub const UPDATE_NODE_ANNOTATION: ActionType = ActionType(1 << 6);
    pub const UPDATE_POD_LABEL: ActionType = ActionType(1 << 7);
    pub const UPDATE_POD_SCALE_DOWN: ActionType = ActionType(1 << 8);
    pub const UPDATE_POD_TOLERATION: ActionType = ActionType(1 << 9);
    pub const UPDATE_POD_SCHEDULING_GATES_ELIMINATED: ActionType = ActionType(1 << 10);
    pub const UPDATE_POD_GENERATED_RESOURCE_CLAIM: ActionType = ActionType(1 << 11);
    /// Upstream's unexported `updatePodOther` — any pod change that isn't one
    /// of the specific bits above. Kept so `UPDATE` remains "every kind of
    /// update" and a plugin can still ask for the catch-all if it must.
    pub const UPDATE_POD_OTHER: ActionType = ActionType(1 << 12);

    /// Every update bit, for a plugin that genuinely cannot be more specific.
    pub const UPDATE: ActionType = ActionType(
        Self::UPDATE_NODE_ALLOCATABLE.0
            | Self::UPDATE_NODE_LABEL.0
            | Self::UPDATE_NODE_TAINT.0
            | Self::UPDATE_NODE_CONDITION.0
            | Self::UPDATE_NODE_ANNOTATION.0
            | Self::UPDATE_POD_LABEL.0
            | Self::UPDATE_POD_SCALE_DOWN.0
            | Self::UPDATE_POD_TOLERATION.0
            | Self::UPDATE_POD_SCHEDULING_GATES_ELIMINATED.0
            | Self::UPDATE_POD_GENERATED_RESOURCE_CLAIM.0
            | Self::UPDATE_POD_OTHER.0,
    );

    pub const ALL: ActionType = ActionType(Self::ADD.0 | Self::DELETE.0 | Self::UPDATE.0);

    /// Whether these two masks overlap at all — the match test used when
    /// deciding whether an event can wake a plugin.
    pub fn intersects(self, other: ActionType) -> bool {
        self.0 & other.0 != 0
    }

    pub fn is_empty(self) -> bool {
        self.0 == 0
    }

    pub fn contains(self, other: ActionType) -> bool {
        self.0 & other.0 == other.0
    }
}

impl std::ops::BitOr for ActionType {
    type Output = ActionType;
    fn bitor(self, rhs: ActionType) -> ActionType {
        ActionType(self.0 | rhs.0)
    }
}

impl std::ops::BitOrAssign for ActionType {
    fn bitor_assign(&mut self, rhs: ActionType) {
        self.0 |= rhs.0;
    }
}

/// A resource changed. This is what plugins subscribe to and what the queue
/// matches against.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ClusterEvent {
    pub resource: EventResource,
    pub action: ActionType,
}

impl ClusterEvent {
    pub const fn new(resource: EventResource, action: ActionType) -> Self {
        Self { resource, action }
    }

    /// Whether an incoming `event` can wake a plugin that registered `self`.
    ///
    /// WildCard on either side matches any resource; the action masks must
    /// overlap. Both halves matter: a plugin registered for
    /// `Node/UpdateNodeTaint` must not be woken by a Node label change.
    pub fn matches(&self, event: &ClusterEvent) -> bool {
        let resource_ok = self.resource == EventResource::WildCard
            || event.resource == EventResource::WildCard
            || self.resource == event.resource
            // A plugin that registered for Pod generally is woken by both
            // flavours of pod event; one that named a flavour is not woken by
            // the other.
            || (self.resource == EventResource::Pod
                && matches!(
                    event.resource,
                    EventResource::AssignedPod | EventResource::UnschedulablePod
                ));
        resource_ok && self.action.intersects(event.action)
    }
}

/// The safety net's event: matches everything, so every stranded pod is
/// reconsidered. See docs/SCHEDULER.md — if this ever actually rescues a pod,
/// a QueueingHint is incomplete.
pub const UNSCHEDULABLE_TIMEOUT: ClusterEvent =
    ClusterEvent::new(EventResource::WildCard, ActionType::ALL);

/// A plugin explicitly asked for these pods to be reconsidered now.
pub const FORCE_ACTIVATE: ClusterEvent =
    ClusterEvent::new(EventResource::WildCard, ActionType::ALL);

// ─────────────────────────────────────────────────────────────────────────
// The diffs
// ─────────────────────────────────────────────────────────────────────────

/// What actually changed between two versions of a Node.
///
/// Returns [`ActionType::NONE`] for the overwhelmingly common case — a status
/// heartbeat that moved only `lastHeartbeatTime`. See the module header for
/// why that single fact is the point of this file.
pub fn node_action_types(old: &Node, new: &Node) -> ActionType {
    let mut action = ActionType::NONE;

    if old.metadata.labels != new.metadata.labels {
        action |= ActionType::UPDATE_NODE_LABEL;
    }
    if old.metadata.annotations != new.metadata.annotations {
        action |= ActionType::UPDATE_NODE_ANNOTATION;
    }

    let old_taints = old.spec.as_ref().and_then(|s| s.taints.as_ref());
    let new_taints = new.spec.as_ref().and_then(|s| s.taints.as_ref());
    if old_taints != new_taints {
        action |= ActionType::UPDATE_NODE_TAINT;
    }
    // `spec.unschedulable` is a scheduling input (NodeUnschedulable filters on
    // it), and upstream folds cordon/uncordon into the taint bit because the
    // node controller also mirrors it as a taint. Doing the same keeps a
    // cordon from being missed on a cluster where that mirroring is off.
    let old_unsched = old.spec.as_ref().and_then(|s| s.unschedulable);
    let new_unsched = new.spec.as_ref().and_then(|s| s.unschedulable);
    if old_unsched != new_unsched {
        action |= ActionType::UPDATE_NODE_TAINT;
    }

    let old_status = old.status.as_ref();
    let new_status = new.status.as_ref();

    if old_status.and_then(|s| s.allocatable.as_ref())
        != new_status.and_then(|s| s.allocatable.as_ref())
    {
        action |= ActionType::UPDATE_NODE_ALLOCATABLE;
    }

    // THE hot path. Compare only (type, status) pairs — never the whole
    // condition, which carries lastHeartbeatTime and changes every 10s per
    // node while meaning nothing to any scheduling decision.
    if node_condition_statuses(old) != node_condition_statuses(new) {
        action |= ActionType::UPDATE_NODE_CONDITION;
    }

    action
}

/// `(type, status)` for every condition, order-independent.
///
/// A BTreeMap rather than a Vec because the apiserver makes no promise about
/// condition ordering, and a reordering is not a change.
fn node_condition_statuses(node: &Node) -> BTreeMap<&str, &str> {
    node.status
        .as_ref()
        .and_then(|s| s.conditions.as_ref())
        .map(|cs| {
            cs.iter()
                .map(|c| (c.type_.as_str(), c.status.as_str()))
                .collect()
        })
        .unwrap_or_default()
}

/// What actually changed between two versions of a Pod.
///
/// Pods churn nearly as hard as nodes (every container status update, every
/// probe result), and almost none of it is a scheduling input. Anything not
/// recognised as a specific bit becomes `UPDATE_POD_OTHER` rather than
/// nothing, because unlike node heartbeats there is no single dominant
/// benign pod update to special-case, and silently dropping a real change
/// strands pods.
pub fn pod_action_types(old: &Pod, new: &Pod) -> ActionType {
    let mut action = ActionType::NONE;

    if old.metadata.labels != new.metadata.labels {
        action |= ActionType::UPDATE_POD_LABEL;
    }

    let old_spec = old.spec.as_ref();
    let new_spec = new.spec.as_ref();

    if old_spec.and_then(|s| s.tolerations.as_ref()) != new_spec.and_then(|s| s.tolerations.as_ref())
    {
        action |= ActionType::UPDATE_POD_TOLERATION;
    }

    // Gates only ever shrink (the API forbids adding one after creation), so
    // "was non-empty, is now empty" is the transition that makes a gated pod
    // schedulable. SchedulingGates' PreEnqueue subscribes to exactly this.
    let old_gates = old_spec.and_then(|s| s.scheduling_gates.as_ref()).map_or(0, |g| g.len());
    let new_gates = new_spec.and_then(|s| s.scheduling_gates.as_ref()).map_or(0, |g| g.len());
    if old_gates > 0 && new_gates == 0 {
        action |= ActionType::UPDATE_POD_SCHEDULING_GATES_ELIMINATED;
    }

    // In-place vertical scaling: a pod whose requests shrank frees capacity
    // on its node, which can un-stick other pods. Only a *decrease* matters
    // for that purpose.
    if pod_requests_decreased(old, new) {
        action |= ActionType::UPDATE_POD_SCALE_DOWN;
    }

    // DRA: the resourceclaim controller materialising a generated claim is
    // what lets DynamicResources' PreEnqueue admit the pod.
    if old.status.as_ref().and_then(|s| s.resource_claim_statuses.as_ref())
        != new.status.as_ref().and_then(|s| s.resource_claim_statuses.as_ref())
    {
        action |= ActionType::UPDATE_POD_GENERATED_RESOURCE_CLAIM;
    }

    if action.is_empty() && pod_changed_otherwise(old, new) {
        action |= ActionType::UPDATE_POD_OTHER;
    }

    action
}

/// Whether any container's requests got smaller.
fn pod_requests_decreased(old: &Pod, new: &Pod) -> bool {
    let old_r = crate::cache::pod::pod_requests(old);
    let new_r = crate::cache::pod::pod_requests(new);
    // A key the new spec dropped entirely is a decrease too — from whatever
    // it was to zero — the same as a key whose value shrank.
    old_r
        .names()
        .into_iter()
        .any(|name| new_r.get(&name) < old_r.get(&name))
}

/// Did anything a scheduler could plausibly care about change, beyond the
/// specific bits already checked? Deliberately conservative.
fn pod_changed_otherwise(old: &Pod, new: &Pod) -> bool {
    old.spec != new.spec
        || old.metadata.annotations != new.metadata.annotations
        || old.metadata.owner_references != new.metadata.owner_references
}

#[cfg(test)]
#[path = "events_tests.rs"]
mod tests;
