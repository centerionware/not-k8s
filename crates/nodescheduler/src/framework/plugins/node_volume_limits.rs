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
//! PVC's StorageClass provisioner while it is still unbound). Identity is the
//! CSI driver plus volume handle, not the PVC name: one volume mounted by two
//! pods consumes one attachment slot. Direct inline CSI volumes are not
//! attachable and therefore are not counted. Lingering VolumeAttachments are
//! counted even after their pod has gone, matching upstream.
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

/// Unique volume identity -> CSI driver.
struct WantedVolumes(HashMap<String, String>);
/// Every node's already-attached unique volumes.
struct AttachedVolumesByNode(HashMap<String, HashMap<String, String>>);
/// Every node's own `CSINode`-reported ceiling, by driver. `None` for a
/// driver means "reported, no limit"; a driver absent from the inner map was
/// never registered on that node at all, which this plugin does not enforce
/// against — that is a mount-time failure for the driver to report, not a
/// scheduling-time one.
struct LimitsByNodeAndDriver(HashMap<String, std::collections::BTreeMap<String, Option<i32>>>);

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
                ActionType::ADD | ActionType::UPDATE,
            )),
            // A driver's own reported ceiling can change (e.g. after the
            // MutableCSINodeAllocatableCount feature updates it), and a new
            // node starts with none attached at all.
            ClusterEventWithHint::always(ClusterEvent::new(
                EventResource::CsiNode,
                ActionType::ADD | ActionType::UPDATE,
            )),
            ClusterEventWithHint::always(ClusterEvent::new(
                EventResource::VolumeAttachment,
                ActionType::DELETE,
            )),
        ]
    }
}

/// Resolve one pod's attachable volumes. Bound claims use the real CSI
/// handle; an unbound delayed-binding claim gets a collision-proof synthetic
/// identity until a PV exists, just as upstream does.
fn volumes_for_pod(pod: &PodInfo, snapshot: &Snapshot) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for pvc_name in &pod.pvc_names {
        let Some(pvc) = snapshot.pvc(&pod.namespace, pvc_name) else {
            continue;
        };
        if let Some(pv) = pvc.volume_name.as_ref().and_then(|name| snapshot.pv(name)) {
            if let (Some(driver), Some(handle)) = (&pv.csi_driver, &pv.csi_volume_handle) {
                if !driver.is_empty() && !handle.is_empty() {
                    out.insert(format!("{driver}/{handle}"), driver.clone());
                    continue;
                }
            }
        }
        if let Some(driver) = pvc
            .storage_class_name
            .as_ref()
            .and_then(|name| snapshot.storage_class(name))
            .map(|class| class.provisioner.clone())
            .filter(|driver| !driver.is_empty())
        {
            out.insert(format!("pvc:{}/{}", pod.namespace, pvc_name), driver);
        }
    }
    out
}

fn attached_volumes_by_node(snapshot: &Snapshot) -> HashMap<String, HashMap<String, String>> {
    let mut by_node = HashMap::new();
    for node in snapshot.nodes() {
        let attached = by_node.entry(node.name.clone()).or_insert_with(HashMap::new);
        for pod in &node.pods {
            attached.extend(volumes_for_pod(pod, snapshot));
        }
    }
    for attachment in snapshot.volume_attachments.values() {
        let Some(pv) = attachment.pv_name.as_ref().and_then(|name| snapshot.pv(name)) else {
            continue;
        };
        let Some(handle) = pv.csi_volume_handle.as_ref().filter(|handle| !handle.is_empty()) else {
            continue;
        };
        let driver = pv
            .csi_driver
            .as_deref()
            .filter(|driver| !driver.is_empty())
            .unwrap_or(attachment.attacher.as_str());
        if driver.is_empty() {
            continue;
        }
        by_node
            .entry(attachment.node_name.clone())
            .or_insert_with(HashMap::new)
            .insert(
                format!("{driver}/{handle}"),
                driver.to_string(),
            );
    }
    by_node
}

fn limits_by_node(snapshot: &Snapshot) -> HashMap<String, std::collections::BTreeMap<String, Option<i32>>> {
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
        let wanted = volumes_for_pod(pod, snapshot);
        if wanted.is_empty() {
            state.skip_filter(NAME);
            return (Status::skip(), None);
        }
        state.write(NAME, WantedVolumes(wanted));
        state.write(ATTACHED_KEY, AttachedVolumesByNode(attached_volumes_by_node(snapshot)));
        state.write(LIMITS_KEY, LimitsByNodeAndDriver(limits_by_node(snapshot)));
        (Status::success(), None)
    }
}

impl FilterPlugin for NodeVolumeLimits {
    fn filter(&self, state: &CycleState, _pod: &PodInfo, node: &NodeInfo) -> Status {
        let Some(wanted) = state.read::<WantedVolumes>(NAME) else {
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
        let attached = state.read::<AttachedVolumesByNode>(ATTACHED_KEY).and_then(|a| a.0.get(&node.name));
        let mut already_by_driver: HashMap<&str, usize> = HashMap::new();
        for driver in attached.into_iter().flat_map(|volumes| volumes.values()) {
            *already_by_driver.entry(driver).or_default() += 1;
        }
        let mut wanted_by_driver: HashMap<&str, usize> = HashMap::new();
        for (identity, driver) in &wanted.0 {
            if attached.is_some_and(|volumes| volumes.contains_key(identity)) {
                continue;
            }
            *wanted_by_driver.entry(driver).or_default() += 1;
        }

        for (driver, want) in wanted_by_driver {
            let Some(limit) = limits.get(driver) else {
                // This node's CSINode never registered the driver at all.
                continue;
            };
            let Some(limit) = limit else {
                // Registered with no reported ceiling: unbounded.
                continue;
            };
            let already = already_by_driver.get(driver).copied().unwrap_or(0);
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
