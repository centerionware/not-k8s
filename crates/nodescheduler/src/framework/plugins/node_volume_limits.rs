//! `NodeVolumeLimits` — the CSI (only) successor to the removed in-tree
//! `EBSLimits`/`GCEPDLimits`/`AzureDiskLimits` filters. See docs/SCHEDULER.md,
//! "Where the docs are wrong": those three are gone from `release-1.33`'s
//! default plugin set entirely, and this is the one that replaced them.
//!
//! # What "a volume" means here
//!
//! A node can only attach so many volumes for a given CSI driver —
//! `CSINode.spec.drivers[].allocatable.count`, which the driver itself
//! reports. Two different mounts fold into one ceiling: a pod's PVC-backed
//! volumes (resolved through the PVC to the PV it is bound to, or through the
//! PVC's StorageClass provisioner while it is still unbound) and its
//! ephemeral inline CSI volumes
//! ([`crate::cache::PodInfo::csi_ephemeral_drivers`]) — a driver cannot tell
//! the two apart once attached, so neither can this.
//!
//! # Why the per-node counts and limits are computed once, not once per node
//!
//! `FilterPlugin::filter` is deliberately not handed the `Snapshot` — see
//! `framework/mod.rs`'s header, "per node, run in parallel". Counting how many
//! volumes of each driver are already on *every* node, and reading every
//! node's own reported ceiling, therefore both have to happen in `PreFilter`,
//! which does see the whole snapshot; the results go into `CycleState` for
//! `Filter` to index into by node name. That is a single O(cluster pods)
//! pass per scheduling cycle rather than one per node, which is the cheaper
//! direction as the cluster grows.

use crate::cache::{NodeInfo, PodInfo, Snapshot};
use crate::events::{ActionType, ClusterEvent, EventResource};
use crate::framework::status::Status;
use crate::framework::{ClusterEventWithHint, CycleState, FilterPlugin, Plugin, PreFilterPlugin};
use std::collections::HashMap;

pub const NAME: &str = "NodeVolumeLimits";

/// A second `CycleState` key so this and [`WantedByDriver`] — different types
/// — don't collide under one plugin name.
const ATTACHED_KEY: &str = "NodeVolumeLimits/attached";
/// A third key for each node's own reported per-driver ceiling.
const LIMITS_KEY: &str = "NodeVolumeLimits/limits";

/// This pod's new volumes, tallied by driver.
struct WantedByDriver(HashMap<String, usize>);
/// Every node's already-attached volumes, tallied by driver.
struct AttachedByNodeAndDriver(HashMap<String, HashMap<String, usize>>);
/// Every node's own `CSINode`-reported ceiling, by driver. `None` for a
/// driver means "reported, no limit"; a driver absent from the inner map was
/// never registered on that node at all, which this plugin does not enforce
/// against — that is a mount-time failure for the driver to report, not a
/// scheduling-time one.
struct LimitsByNodeAndDriver(HashMap<String, HashMap<String, Option<i32>>>);

#[derive(Default)]
pub struct NodeVolumeLimits;

impl Plugin for NodeVolumeLimits {
    fn name(&self) -> &'static str {
        NAME
    }

    fn events_to_register(&self) -> Vec<ClusterEventWithHint> {
        vec![
            // A volume-holding pod leaving a node is the ordinary way this
            // resolves.
            ClusterEventWithHint::always(ClusterEvent::new(
                EventResource::AssignedPod,
                ActionType::DELETE,
            )),
            // A PVC finishing its bind can change which driver its volume
            // resolves to (from "unknown, guess the provisioner" to "known,
            // read the PV") and is otherwise the only way an unbound PVC's
            // rejection here could change.
            ClusterEventWithHint::always(ClusterEvent::new(
                EventResource::PersistentVolumeClaim,
                ActionType::UPDATE,
            )),
            // A driver's own reported ceiling can change (e.g. after the
            // MutableCSINodeAllocatableCount feature updates it), and a new
            // node starts with none attached at all.
            ClusterEventWithHint::always(ClusterEvent::new(
                EventResource::CsiNode,
                ActionType::ADD | ActionType::UPDATE,
            )),
        ]
    }
}

/// Resolve one pod's CSI drivers, keyed so a PVC mounted twice by the same
/// pod counts once but two different PVCs count separately.
fn driver_counts_for_pod(pod: &PodInfo, snapshot: &Snapshot) -> HashMap<String, usize> {
    let mut out: HashMap<String, usize> = HashMap::new();
    for pvc_name in &pod.pvc_names {
        let Some(pvc) = snapshot.pvc(&pod.namespace, pvc_name) else {
            continue;
        };
        let driver = pvc
            .volume_name
            .as_ref()
            .and_then(|vn| snapshot.pv(vn))
            .and_then(|pv| pv.csi_driver.clone())
            .or_else(|| {
                pvc.storage_class_name
                    .as_ref()
                    .and_then(|sc| snapshot.storage_class(sc))
                    .map(|sc| sc.provisioner.clone())
            });
        if let Some(driver) = driver {
            *out.entry(driver).or_default() += 1;
        }
    }
    for driver in &pod.csi_ephemeral_drivers {
        *out.entry(driver.clone()).or_default() += 1;
    }
    out
}

fn attached_counts_by_node(snapshot: &Snapshot) -> HashMap<String, HashMap<String, usize>> {
    snapshot
        .nodes()
        .iter()
        .map(|n| {
            let mut totals: HashMap<String, usize> = HashMap::new();
            for p in &n.pods {
                for (driver, count) in driver_counts_for_pod(p, snapshot) {
                    *totals.entry(driver).or_default() += count;
                }
            }
            (n.name.clone(), totals)
        })
        .collect()
}

fn limits_by_node(snapshot: &Snapshot) -> HashMap<String, HashMap<String, Option<i32>>> {
    snapshot
        .nodes()
        .iter()
        .filter_map(|n| snapshot.csi_node(&n.name).map(|csi| (n.name.clone(), csi.drivers.clone())))
        .collect()
}

impl PreFilterPlugin for NodeVolumeLimits {
    fn pre_filter(
        &self,
        state: &mut CycleState,
        pod: &PodInfo,
        snapshot: &Snapshot,
    ) -> (Status, Option<Vec<String>>) {
        let wanted = driver_counts_for_pod(pod, snapshot);
        if wanted.is_empty() {
            state.skip_filter(NAME);
            return (Status::skip(), None);
        }
        state.write(NAME, WantedByDriver(wanted));
        state.write(ATTACHED_KEY, AttachedByNodeAndDriver(attached_counts_by_node(snapshot)));
        state.write(LIMITS_KEY, LimitsByNodeAndDriver(limits_by_node(snapshot)));
        (Status::success(), None)
    }
}

impl FilterPlugin for NodeVolumeLimits {
    fn filter(&self, state: &CycleState, _pod: &PodInfo, node: &NodeInfo) -> Status {
        let Some(wanted) = state.read::<WantedByDriver>(NAME) else {
            // No PreFilter state — the preemption dry-run path builds a fresh
            // `CycleState` per hypothetical set and does not re-run PreFilter
            // for this plugin, or this cycle's pod had no CSI volumes at all
            // and PreFilter already skipped Filter entirely.
            return Status::success();
        };
        let Some(limits) = state.read::<LimitsByNodeAndDriver>(LIMITS_KEY).and_then(|l| l.0.get(&node.name))
        else {
            // No CSINode for this node at all: nothing reported a ceiling, so
            // there is nothing to enforce.
            return Status::success();
        };
        let attached = state.read::<AttachedByNodeAndDriver>(ATTACHED_KEY).and_then(|a| a.0.get(&node.name));

        for (driver, want) in &wanted.0 {
            let Some(limit) = limits.get(driver) else {
                // This node's CSINode never registered the driver at all.
                continue;
            };
            let Some(limit) = limit else {
                // Registered with no reported ceiling: unbounded.
                continue;
            };
            let already = attached.and_then(|a| a.get(driver)).copied().unwrap_or(0);
            if already + want > *limit as usize {
                return Status::unschedulable(
                    NAME,
                    format!("node(s) exceed max volume count for driver {driver:?}"),
                );
            }
        }
        Status::success()
    }
}

#[cfg(test)]
#[path = "node_volume_limits_tests.rs"]
mod tests;
