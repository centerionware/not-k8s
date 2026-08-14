//! `VolumeRestrictions` — two unrelated conflict rules that happen to share a
//! name upstream: legacy in-tree volume identity conflicts (per node), and
//! `ReadWriteOncePod` exclusivity (cluster-wide).
//!
//! # Why these are checked in different extension points
//!
//! The legacy check (GCE PD, AWS EBS, iSCSI, RBD, Cinder — see
//! [`crate::cache::LegacyVolumeId`]) is a **per-node** question: can this
//! pod's volumes coexist with what is *already on this specific node*. That
//! is what `Filter` is for.
//!
//! `ReadWriteOncePod` is not. The access mode means "at most one pod, cluster
//! wide, regardless of node" — a PVC using it conflicts with a pod on a node
//! nowhere near the one being filtered just as much as one on the same node.
//! Since the answer does not depend on which node is being asked, it is
//! computed exactly once, in `PreFilter`: either there is no conflict
//! anywhere and every node proceeds, or there is one and the whole pod is
//! rejected before any node-by-node work happens at all. Upstream reaches the
//! same place through its own PVC lister; this reads it off the snapshot
//! (`Snapshot::pods_using_pvc`), which is the pattern this crate always uses
//! instead of a second live index into the same data.
//!
//! An `UnschedulableAndUnresolvable` PreFilter rejection here is deliberate,
//! not a shortcut: whichever pod holds the PVC has to be freed for this one
//! to ever fit, and that happens through the same `AssignedPod` delete event
//! this plugin already subscribes to for the legacy check — no separate
//! preemption-extension machinery is needed because the rejection already
//! carries the one event that can undo it.

use crate::cache::{LegacyVolumeId, NodeInfo, PodInfo, Snapshot};
use crate::events::{ActionType, ClusterEvent, EventResource};
use crate::framework::status::Status;
use crate::framework::{ClusterEventWithHint, CycleState, FilterPlugin, Plugin, PreFilterPlugin};

pub const NAME: &str = "VolumeRestrictions";

struct WantedLegacyVolumes(Vec<LegacyVolumeId>);

#[derive(Default)]
pub struct VolumeRestrictions;

impl Plugin for VolumeRestrictions {
    fn name(&self) -> &'static str {
        NAME
    }

    fn events_to_register(&self) -> Vec<ClusterEventWithHint> {
        // Both rejection paths this plugin can produce are undone the same
        // way: the pod holding the conflicting volume (legacy identity or the
        // ReadWriteOncePod claim) goes away.
        vec![ClusterEventWithHint::always(ClusterEvent::new(
            EventResource::AssignedPod,
            ActionType::DELETE,
        ))]
    }
}

/// Whether `pod` conflicts with an existing user of a `ReadWriteOncePod` PVC
/// it also wants to use — cluster-wide, per the module header.
fn read_write_once_pod_conflict(pod: &PodInfo, snapshot: &Snapshot) -> Option<String> {
    for pvc_name in &pod.pvc_names {
        let Some(pvc) = snapshot.pvc(&pod.namespace, pvc_name) else {
            continue;
        };
        if !pvc.wants_read_write_once_pod() {
            continue;
        }
        if let Some(other) = snapshot
            .pods_using_pvc(&pod.namespace, pvc_name)
            .find(|p| p.uid != pod.uid)
        {
            return Some(format!(
                "persistentvolumeclaim {pvc_name:?} is ReadWriteOncePod and already in use by pod {}",
                other.key()
            ));
        }
    }
    None
}

impl PreFilterPlugin for VolumeRestrictions {
    fn pre_filter(
        &self,
        state: &mut CycleState,
        pod: &PodInfo,
        snapshot: &Snapshot,
    ) -> (Status, Option<Vec<String>>) {
        if let Some(reason) = read_write_once_pod_conflict(pod, snapshot) {
            return (Status::unresolvable(NAME, reason), None);
        }

        if pod.legacy_volumes.is_empty() {
            state.skip_filter(NAME);
            return (Status::skip(), None);
        }
        state.write(NAME, WantedLegacyVolumes(pod.legacy_volumes.clone()));
        (Status::success(), None)
    }
}

impl FilterPlugin for VolumeRestrictions {
    fn filter(&self, state: &CycleState, pod: &PodInfo, node: &NodeInfo) -> Status {
        let wanted: &[LegacyVolumeId] = match state.read::<WantedLegacyVolumes>(NAME) {
            Some(w) => &w.0,
            None => &pod.legacy_volumes,
        };
        if wanted.is_empty() {
            return Status::success();
        }
        for existing in &node.pods {
            for have in &existing.legacy_volumes {
                if wanted.iter().any(|want| want.conflicts_with(have)) {
                    return Status::unschedulable(
                        NAME,
                        "node(s) had volume node conflict",
                    );
                }
            }
        }
        Status::success()
    }
}

#[cfg(test)]
#[path = "volume_restrictions_tests.rs"]
mod tests;
