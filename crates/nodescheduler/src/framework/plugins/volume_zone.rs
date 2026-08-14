//! `VolumeZone` — a pod's bound PersistentVolumes must be reachable from the
//! node it lands on, for volumes whose zone constraint is expressed as a
//! **label** on the PV rather than a `nodeAffinity`.
//!
//! # Why this only reads labels, and `VolumeBinding` does the rest
//!
//! Two generations of the same idea coexist in the API. The original one
//! predates CSI: an in-tree PV carries `topology.kubernetes.io/zone` (or the
//! deprecated `failure-domain.beta.kubernetes.io/zone`) as a plain label, and
//! nothing else ties it to a zone. The newer one is `spec.nodeAffinity`, which
//! is how CSI topology-aware provisioning expresses the same constraint and
//! is upstream's own `VolumeBinding` plugin's job to check
//! (`checkNodeAffinity`) — duplicating that here would check the same PV
//! twice through two different plugins and call it parity. So this plugin's
//! whole surface is the label-only case; a PV that only has `nodeAffinity`
//! and no zone label passes through here with nothing to say.
//!
//! A mismatch here is unresolvable: the node's zone is not going to change,
//! and neither is the PV's, so no amount of evicting other pods helps.

use crate::cache::{NodeInfo, PodInfo, Snapshot};
use crate::events::{ActionType, ClusterEvent, EventResource};
use crate::framework::status::Status;
use crate::framework::{ClusterEventWithHint, CycleState, FilterPlugin, Plugin, PreFilterPlugin};
use std::collections::BTreeMap;

pub const NAME: &str = "VolumeZone";

const ZONE_KEYS: [&str; 2] =
    ["topology.kubernetes.io/zone", "failure-domain.beta.kubernetes.io/zone"];
const REGION_KEYS: [&str; 2] =
    ["topology.kubernetes.io/region", "failure-domain.beta.kubernetes.io/region"];

/// The first of a set of equivalent keys present on a label map.
fn first(labels: &BTreeMap<String, String>, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|k| labels.get(*k).cloned())
}

/// This pod's bound PVs' zone/region constraints, resolved once per cycle.
struct WantedZones(Vec<(Option<String>, Option<String>)>);

#[derive(Default)]
pub struct VolumeZone;

impl Plugin for VolumeZone {
    fn name(&self) -> &'static str {
        NAME
    }

    fn events_to_register(&self) -> Vec<ClusterEventWithHint> {
        // A rejection here changes only if the PVC ends up bound to a
        // different PV, or a PV's labels change (a real but rare edit).
        vec![
            ClusterEventWithHint::always(ClusterEvent::new(
                EventResource::PersistentVolumeClaim,
                ActionType::UPDATE,
            )),
            ClusterEventWithHint::always(ClusterEvent::new(
                EventResource::PersistentVolume,
                ActionType::ADD | ActionType::UPDATE,
            )),
        ]
    }
}

impl PreFilterPlugin for VolumeZone {
    fn pre_filter(
        &self,
        state: &mut CycleState,
        pod: &PodInfo,
        snapshot: &Snapshot,
    ) -> (Status, Option<Vec<String>>) {
        let mut wanted = Vec::new();
        for pvc_name in &pod.pvc_names {
            let Some(pvc) = snapshot.pvc(&pod.namespace, pvc_name) else { continue };
            let Some(pv) = pvc.volume_name.as_ref().and_then(|vn| snapshot.pv(vn)) else {
                // Not bound yet — `VolumeBinding` owns getting it bound, and
                // there is nothing to check against until it is.
                continue;
            };
            let zone = first(&pv.labels, &ZONE_KEYS);
            let region = first(&pv.labels, &REGION_KEYS);
            if zone.is_some() || region.is_some() {
                wanted.push((zone, region));
            }
        }
        if wanted.is_empty() {
            state.skip_filter(NAME);
            return (Status::skip(), None);
        }
        state.write(NAME, WantedZones(wanted));
        (Status::success(), None)
    }
}

impl FilterPlugin for VolumeZone {
    fn filter(&self, state: &CycleState, _pod: &PodInfo, node: &NodeInfo) -> Status {
        let Some(wanted) = state.read::<WantedZones>(NAME) else {
            return Status::success();
        };
        let node_zone = first(&node.labels, &ZONE_KEYS);
        let node_region = first(&node.labels, &REGION_KEYS);

        for (zone, region) in &wanted.0 {
            if let Some(z) = zone {
                if node_zone.as_deref() != Some(z.as_str()) {
                    return Status::unresolvable(NAME, "node(s) had no available volume zone");
                }
            }
            if let Some(r) = region {
                if node_region.as_deref() != Some(r.as_str()) {
                    return Status::unresolvable(NAME, "node(s) had no available volume zone");
                }
            }
        }
        Status::success()
    }
}

#[cfg(test)]
#[path = "volume_zone_tests.rs"]
mod tests;
