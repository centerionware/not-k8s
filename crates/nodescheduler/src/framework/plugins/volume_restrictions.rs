//! `VolumeRestrictions` — two unrelated conflict rules that happen to share a
//! name upstream: legacy in-tree volume identity conflicts (per node), and
//! `ReadWriteOncePod` exclusivity (cluster-wide, but resolved per node — see
//! below).
//!
//! # Why the legacy check and the RWOP check are still checked differently
//!
//! The legacy check (GCE PD, AWS EBS, iSCSI, RBD, Cinder — see
//! [`crate::cache::LegacyVolumeId`]) is a **per-node** question: can this
//! pod's volumes coexist with what is *already on this specific node*. That
//! is what `Filter` is for, straightforwardly.
//!
//! `ReadWriteOncePod` is not per-node in the same sense — the access mode
//! means "at most one pod, cluster wide, regardless of node" — but it is
//! still checked in `Filter`, not rejected outright in `PreFilter`, and the
//! difference matters:
//!
//! An earlier version of this plugin rejected in `PreFilter` with
//! `UnschedulableAndUnresolvable` the moment a conflict was found, before any
//! node was even considered. That was wrong, not merely conservative: it made
//! the rejection *look* resolvable-by-preemption in every other respect
//! except that no preemption ever ran — the whole cycle bailed before Filter,
//! PostFilter, or victim selection got a chance to run at all. Upstream's own
//! `volumerestrictions.go` computes the same state in `PreFilter` but only
//! *rejects* in `Filter`, returning plain `Unschedulable` (resolvable) — and
//! that is what lets preemption actually free the conflicting PVC by evicting
//! its current holder.
//!
//! So here too: `PreFilter` computes, once per cycle, which of `pod`'s own
//! RWOP PVCs are currently held by a different pod (via
//! `Snapshot::pods_using_pvc`, this crate's index over the same relationship
//! upstream's PVC lister answers). `Filter` then rejects — identically for
//! every node, since the conflict does not depend on which node is asked —
//! whenever that per-cycle state still shows a conflict. And because the
//! state can change *within* a cycle (preemption's dry run in `cycle.rs`'s
//! `fits_without` hypothetically removes a candidate victim and re-runs
//! `Filter`), this plugin implements [`crate::framework::PreFilterExtensions`]
//! to keep that state honest: if the hypothetically-removed pod was the one
//! holding the conflicting PVC, `remove_pod` clears the conflict so `Filter`
//! can pass, and `add_pod` restores it afterward, undoing the simulation.
//! Without this, a Filter-based RWOP check would be resolvable in name only —
//! preemption could evict the exact right pod and the rejection would still
//! never lift, because nothing would tell the cached PreFilter state that
//! anything had changed.

use crate::cache::{LegacyVolumeId, NodeInfo, PodInfo, Snapshot};
use crate::events::{ActionType, ClusterEvent, EventResource};
use crate::framework::status::Status;
use crate::framework::{
    ClusterEventWithHint, CycleState, FilterPlugin, Plugin, PreFilterExtensions, PreFilterPlugin,
};
use std::collections::{HashMap, HashSet};

pub const NAME: &str = "VolumeRestrictions";

/// Everything this plugin's `Filter` needs, computed once per cycle by
/// `PreFilter` and kept current across preemption's dry run by
/// `add_pod`/`remove_pod`. One struct, one `CycleState` slot — see
/// `CycleState`'s own doc comment on why two writes under this plugin's name
/// would silently clobber each other.
#[derive(Default, Clone)]
struct VolumeRestrictionsState {
    wanted_legacy: Vec<LegacyVolumeId>,
    /// Names of `pod`'s own PVCs that use `ReadWriteOncePod`. Computed once
    /// from the snapshot in `pre_filter` — `add_pod`/`remove_pod` don't
    /// receive the snapshot, so this is what lets them recognize a relevant
    /// victim without re-querying storage.
    rwop_pvc_names: HashSet<String>,
    /// `pvc_name -> rejection message`, one entry per name in
    /// `rwop_pvc_names` that is currently held by a pod other than `pod`
    /// itself. `Filter` rejects — the same way for every node, see the
    /// module header — exactly when this is non-empty.
    conflicts: HashMap<String, String>,
}

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

#[derive(Default)]
pub struct VolumeRestrictions;

fn rwop_conflict_message(pvc_name: &str, holder: &PodInfo) -> String {
    format!(
        "persistentvolumeclaim {pvc_name:?} is ReadWriteOncePod and already in use by pod {}",
        holder.key()
    )
}

/// Shared by `add_pod`/`remove_pod`: does `other` hold one of `pod`'s own
/// RWOP PVCs, and if so, add or drop the corresponding `conflicts` entry.
///
/// `removing` is `true` when `other` is being hypothetically taken away
/// (preemption's dry run considering it as a victim) and `false` when it is
/// being put back (undoing that simulation).
fn adjust(state: &mut CycleState, pod: &PodInfo, other: &PodInfo, removing: bool) {
    let Some(mut s) = state.read::<VolumeRestrictionsState>(NAME).cloned() else {
        return;
    };
    let mut changed = false;
    for pvc_name in &pod.pvc_names {
        if !s.rwop_pvc_names.contains(pvc_name) {
            continue;
        }
        if !other.pvc_names.iter().any(|n| n == pvc_name) {
            continue;
        }
        if removing {
            s.conflicts.remove(pvc_name);
        } else {
            s.conflicts.insert(pvc_name.clone(), rwop_conflict_message(pvc_name, other));
        }
        changed = true;
    }
    if changed {
        state.write(NAME, s);
    }
}

impl PreFilterPlugin for VolumeRestrictions {
    fn pre_filter(
        &self,
        state: &mut CycleState,
        pod: &PodInfo,
        snapshot: &Snapshot,
    ) -> (Status, Option<Vec<String>>) {
        let mut s =
            VolumeRestrictionsState { wanted_legacy: pod.legacy_volumes.clone(), ..Default::default() };

        for pvc_name in &pod.pvc_names {
            let Some(pvc) = snapshot.pvc(&pod.namespace, pvc_name) else {
                continue;
            };
            if !pvc.wants_read_write_once_pod() {
                continue;
            }
            s.rwop_pvc_names.insert(pvc_name.clone());
            if let Some(other) =
                snapshot.pods_using_pvc(&pod.namespace, pvc_name).find(|p| p.uid != pod.uid)
            {
                s.conflicts.insert(pvc_name.clone(), rwop_conflict_message(pvc_name, other));
            }
        }

        if s.wanted_legacy.is_empty() && s.rwop_pvc_names.is_empty() {
            state.skip_filter(NAME);
            return (Status::skip(), None);
        }
        state.write(NAME, s);
        (Status::success(), None)
    }

    fn extensions(&self) -> Option<&dyn PreFilterExtensions> {
        Some(self)
    }
}

impl PreFilterExtensions for VolumeRestrictions {
    fn add_pod(
        &self,
        state: &mut CycleState,
        pod: &PodInfo,
        pod_to_add: &PodInfo,
        _node: &NodeInfo,
    ) -> Status {
        adjust(state, pod, pod_to_add, false);
        Status::success()
    }

    fn remove_pod(
        &self,
        state: &mut CycleState,
        pod: &PodInfo,
        pod_to_remove: &PodInfo,
        _node: &NodeInfo,
    ) -> Status {
        adjust(state, pod, pod_to_remove, true);
        Status::success()
    }
}

impl FilterPlugin for VolumeRestrictions {
    fn filter(&self, state: &CycleState, pod: &PodInfo, node: &NodeInfo) -> Status {
        let owned;
        let s: &VolumeRestrictionsState = match state.read::<VolumeRestrictionsState>(NAME) {
            Some(s) => s,
            None => {
                owned = VolumeRestrictionsState {
                    wanted_legacy: pod.legacy_volumes.clone(),
                    ..Default::default()
                };
                &owned
            }
        };

        // Node-independent: see the module header. A remaining conflict
        // rejects every node identically, and it is `Unschedulable` (not
        // `UnschedulableAndUnresolvable`) precisely because it can be
        // resolved — by evicting whichever pod the message names.
        if let Some(reason) = s.conflicts.values().next() {
            return Status::unschedulable(NAME, reason.clone());
        }

        if s.wanted_legacy.is_empty() {
            return Status::success();
        }
        for existing in &node.pods {
            for have in &existing.legacy_volumes {
                if s.wanted_legacy.iter().any(|want| want.conflicts_with(have)) {
                    return Status::unschedulable(NAME, "node(s) had volume node conflict");
                }
            }
        }
        Status::success()
    }
}

#[cfg(test)]
#[path = "volume_restrictions_tests.rs"]
mod tests;
