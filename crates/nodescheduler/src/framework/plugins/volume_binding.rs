//! `VolumeBinding` — the plugin that actually gets a pod's unbound
//! `PersistentVolumeClaim`s bound, and checks a bound one's node constraint.
//!
//! # Scope: dynamic provisioning, not admin-pre-created static PVs
//!
//! Upstream's real `VolumeBinding` also matches a `PersistentVolumeClaim`
//! against an already-existing, unclaimed `PersistentVolume` (an admin
//! provisions PVs by hand, or a PVC carries a `selector`), and races that
//! against dynamic provisioning at `Reserve` time so two pods cannot both
//! claim the same static PV. That path is real but genuinely rare against a
//! CSI-only cluster — this project's own reference deploy
//! (`deploy/lib/e2e-full-setup.sh`) provisions everything dynamically, the
//! same way almost every CSI driver in the wild is actually used — and
//! implementing it fully means a second scarce-resource race with its own
//! assume cache, on top of everything below. It is not implemented, and is
//! recorded here rather than silently absent: see docs/SCHEDULER.md, "Phase
//! 4", for the honest accounting of what that leaves out and why it is next
//! rather than skipped.
//!
//! What **is** implemented, faithfully:
//!
//!   * an unbound PVC whose `StorageClass` is `Immediate` (or has none) blocks
//!     the pod outright — the PV controller, not this scheduler, is who binds
//!     it, and nothing about *which node* changes that;
//!   * an unbound PVC whose `StorageClass` is `WaitForFirstConsumer` is
//!     checked against that class's `allowedTopologies` and, when the driver
//!     opts into capacity tracking, `CSIStorageCapacity`;
//!   * `PreBind` writes the `volume.kubernetes.io/selected-node` annotation
//!     that tells the external provisioner which node to provision for, then
//!     polls for `status.phase == "Bound"` — the one genuinely blocking wait
//!     in this whole design (see docs/SCHEDULER.md, invariant 4), which is
//!     exactly why it runs in the binding cycle and not the scheduling loop;
//!   * an **already-bound** PVC's PV is still checked against
//!     `spec.nodeAffinity`, which is how CSI topology-aware dynamic
//!     provisioning expresses "this volume only exists in this zone" —
//!     [`super::volume_zone`] checks the same idea expressed as a label
//!     instead, on PVs old enough to predate `nodeAffinity`.
//!
//! # Why `PreFilter` resolves everything the node-facing checks need
//!
//! `FilterPlugin::filter` is not handed the `Snapshot` (see `node_ports.rs`'s
//! header for the general shape of this). Every constraint that needs the PV,
//! PVC, StorageClass or CSIStorageCapacity objects is therefore resolved once
//! in `PreFilter` into a plain `NodeSelector`/`TopologySelectorTerm`/capacity
//! list, and `Filter` does nothing but compare that against the one node it
//! was actually handed.

use crate::cache::{NodeInfo, PodInfo, Snapshot, StorageCapacityInfo};
use crate::events::{ActionType, ClusterEvent, EventResource};
use crate::framework::status::Status;
use crate::framework::{ClusterEventWithHint, CycleState, FilterPlugin, Plugin, PreFilterPlugin};
use k8s_openapi::api::core::v1::{NodeSelector, TopologySelectorTerm};
use std::time::Duration;

pub const NAME: &str = "VolumeBinding";

const POLL_INTERVAL: Duration = Duration::from_secs(2);

/// The annotation an external provisioner watches to learn which node it is
/// provisioning for. Part of the PVC API contract, not invented here.
const SELECTED_NODE_ANNOTATION: &str = "volume.kubernetes.io/selected-node";

/// What a candidate node has to satisfy for one of the pod's PVCs.
enum PvcConstraint {
    /// Already bound. `None` when the PV carries no `nodeAffinity` at all —
    /// which is the common case for a PV that predates topology-aware
    /// provisioning, or one whose zone is expressed as a label instead (see
    /// `volume_zone.rs`).
    Bound(Option<Box<NodeSelector>>),
    /// Unbound, `WaitForFirstConsumer`. Empty `allowed_topologies` means no
    /// restriction. `capacity` is `None` when the driver does not opt into
    /// capacity tracking (`CSIDriver.spec.storageCapacity`) — in that case
    /// the node is assumed to have room, the same as upstream.
    Delayed { allowed_topologies: Vec<TopologySelectorTerm>, capacity: Option<Vec<StorageCapacityInfo>>, requested_bytes: i64 },
}

struct WantedPvcs(Vec<PvcConstraint>);

pub struct VolumeBinding {
    client: kube::Client,
    bind_timeout: Duration,
}

impl VolumeBinding {
    pub fn new(client: kube::Client, bind_timeout: Duration) -> Self {
        Self { client, bind_timeout }
    }
}

impl Plugin for VolumeBinding {
    fn name(&self) -> &'static str {
        NAME
    }

    fn events_to_register(&self) -> Vec<ClusterEventWithHint> {
        events_impl()
    }
}

/// The pure half of `events_to_register` — see `pre_filter_impl`'s doc
/// comment on why these exist: no `&self`, so no `kube::Client` needed to
/// exercise it directly in a test.
fn events_impl() -> Vec<ClusterEventWithHint> {
    vec![
        // The overwhelmingly common resolution: the PVC this pod is waiting
        // on finishes binding, or newly appears.
        ClusterEventWithHint::always(ClusterEvent::new(
            EventResource::PersistentVolumeClaim,
            ActionType::ADD | ActionType::UPDATE,
        )),
        ClusterEventWithHint::always(ClusterEvent::new(
            EventResource::PersistentVolume,
            ActionType::ADD | ActionType::UPDATE,
        )),
        // A StorageClass appearing is what un-stalls a pod rejected for
        // naming one that did not exist yet.
        ClusterEventWithHint::always(ClusterEvent::new(EventResource::StorageClass, ActionType::ADD)),
        ClusterEventWithHint::always(ClusterEvent::new(
            EventResource::CsiStorageCapacity,
            ActionType::ADD | ActionType::UPDATE,
        )),
    ]
}

/// Whether any of a StorageClass's `allowedTopologies` terms accept this
/// node. Terms are ORed; a term's own requirements are ANDed. Empty means no
/// restriction at all.
fn topology_allows(terms: &[TopologySelectorTerm], labels: &std::collections::BTreeMap<String, String>) -> bool {
    if terms.is_empty() {
        return true;
    }
    terms.iter().any(|term| {
        term.match_label_expressions
            .iter()
            .flatten()
            .all(|req| labels.get(&req.key).is_some_and(|v| req.values.contains(v)))
    })
}

impl PreFilterPlugin for VolumeBinding {
    fn pre_filter(
        &self,
        state: &mut CycleState,
        pod: &PodInfo,
        snapshot: &Snapshot,
    ) -> (Status, Option<Vec<String>>) {
        pre_filter_impl(state, pod, snapshot)
    }
}

/// The pure half of `PreFilter` — no `&self`, so it needs no `kube::Client`
/// to exercise, and every case below is tested directly against it.
fn pre_filter_impl(
    state: &mut CycleState,
    pod: &PodInfo,
    snapshot: &Snapshot,
) -> (Status, Option<Vec<String>>) {
    let mut wanted = Vec::new();
        for pvc_name in &pod.pvc_names {
            let Some(pvc) = snapshot.pvc(&pod.namespace, pvc_name) else {
                return (
                    Status::unschedulable(NAME, format!("persistentvolumeclaim {pvc_name:?} not found")),
                    None,
                );
            };

            if pvc.bound {
                let affinity = pvc
                    .volume_name
                    .as_ref()
                    .and_then(|vn| snapshot.pv(vn))
                    .and_then(|pv| pv.node_affinity.clone());
                wanted.push(PvcConstraint::Bound(affinity));
                continue;
            }

            let Some(sc_name) = pvc.storage_class_name.as_ref() else {
                // No StorageClass named at all: nothing will ever bind this,
                // node choice included — same bucket as an Immediate class.
                return (
                    Status::unresolvable(NAME, "pod has unbound immediate PersistentVolumeClaims"),
                    None,
                );
            };
            let Some(sc) = snapshot.storage_class(sc_name) else {
                return (
                    Status::unschedulable(NAME, format!("storageclass {sc_name:?} not found")),
                    None,
                );
            };
            if !sc.wait_for_first_consumer {
                // Immediate binding: the PV controller/external-provisioner
                // does this on its own schedule, not this scheduler's — and
                // it does not depend on which node the pod lands on.
                return (
                    Status::unresolvable(NAME, "pod has unbound immediate PersistentVolumeClaims"),
                    None,
                );
            }

            let capacity = snapshot
                .csi_driver(&sc.provisioner)
                .filter(|d| d.storage_capacity)
                .map(|_| {
                    snapshot
                        .storage_capacities
                        .iter()
                        .filter(|c| c.storage_class_name == *sc_name)
                        .cloned()
                        .collect::<Vec<_>>()
                });

            wanted.push(PvcConstraint::Delayed {
                allowed_topologies: sc.allowed_topologies.clone(),
                capacity,
                requested_bytes: pvc.requested_bytes,
            });
        }

        if wanted.is_empty() {
            state.skip_filter(NAME);
            return (Status::skip(), None);
        }
        state.write(NAME, WantedPvcs(wanted));
        (Status::success(), None)
}

impl FilterPlugin for VolumeBinding {
    fn filter(&self, state: &CycleState, pod: &PodInfo, node: &NodeInfo) -> Status {
        filter_impl(state, pod, node)
    }
}

/// The pure half of `Filter` — see `pre_filter_impl`'s doc comment.
fn filter_impl(state: &CycleState, _pod: &PodInfo, node: &NodeInfo) -> Status {
        let Some(wanted) = state.read::<WantedPvcs>(NAME) else {
            return Status::success();
        };

        for constraint in &wanted.0 {
            match constraint {
                PvcConstraint::Bound(Some(sel)) => {
                    if !crate::framework::plugins::node_affinity::matches_node_selector(sel, node) {
                        return Status::unresolvable(NAME, "node(s) had volume node affinity conflict");
                    }
                }
                PvcConstraint::Bound(None) => {}
                PvcConstraint::Delayed { allowed_topologies, capacity, requested_bytes } => {
                    if !topology_allows(allowed_topologies, &node.labels) {
                        return Status::unschedulable(NAME, "node(s) had volume topology conflict");
                    }
                    if let Some(capacities) = capacity {
                        let available: i64 = capacities
                            .iter()
                            .filter(|c| {
                                c.node_topology
                                    .as_ref()
                                    .is_none_or(|sel| {
                                        crate::framework::plugins::selector::matches_selector(
                                            Some(sel),
                                            &node.labels,
                                        )
                                    })
                            })
                            .filter_map(|c| c.capacity_bytes)
                            .max()
                            .unwrap_or(0);
                        if available < *requested_bytes {
                            return Status::unschedulable(NAME, "node(s) did not have enough free storage");
                        }
                    }
                }
            }
        }
        Status::success()
}

#[async_trait::async_trait]
impl crate::framework::PreBindPlugin for VolumeBinding {
    /// The one genuinely blocking wait in the whole design — see the module
    /// header and docs/SCHEDULER.md invariant 4. It runs on this pod's own
    /// binding-cycle task, so a slow provisioner stalls only this pod.
    async fn pre_bind(&self, pod: &PodInfo, node: &str) -> Status {
        for pvc_name in &pod.pvc_names {
            if let Err(status) = self.bind_one_pvc(pod, pvc_name, node).await {
                return status;
            }
        }
        Status::success()
    }
}

impl VolumeBinding {
    async fn bind_one_pvc(&self, pod: &PodInfo, pvc_name: &str, node: &str) -> Result<(), Status> {
        use k8s_openapi::api::core::v1::PersistentVolumeClaim;
        use kube::api::{Api, Patch, PatchParams};

        let api: Api<PersistentVolumeClaim> = Api::namespaced(self.client.clone(), &pod.namespace);

        let current = api
            .get(pvc_name)
            .await
            .map_err(|e| Status::error(NAME, format!("reading persistentvolumeclaim {pvc_name:?}: {e}")))?;
        if current.status.as_ref().and_then(|s| s.phase.as_deref()) == Some("Bound") {
            return Ok(());
        }

        let already_selected = current
            .metadata
            .annotations
            .as_ref()
            .and_then(|a| a.get(SELECTED_NODE_ANNOTATION))
            .map(|v| v == node)
            .unwrap_or(false);
        if !already_selected {
            let patch = serde_json::json!({
                "metadata": { "annotations": { SELECTED_NODE_ANNOTATION: node } }
            });
            api.patch(
                pvc_name,
                &PatchParams { field_manager: Some("nodescheduler".to_string()), ..Default::default() },
                &Patch::Merge(patch),
            )
            .await
            .map_err(|e| {
                Status::error(NAME, format!("annotating persistentvolumeclaim {pvc_name:?} for node {node}: {e}"))
            })?;
        }

        let deadline = tokio::time::Instant::now() + self.bind_timeout;
        loop {
            let current = api
                .get(pvc_name)
                .await
                .map_err(|e| Status::error(NAME, format!("polling persistentvolumeclaim {pvc_name:?}: {e}")))?;
            if current.status.as_ref().and_then(|s| s.phase.as_deref()) == Some("Bound") {
                return Ok(());
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(Status::unschedulable(
                    NAME,
                    format!(
                        "persistentvolumeclaim {pvc_name:?} did not bind within {}s of selecting node {node}",
                        self.bind_timeout.as_secs()
                    ),
                ));
            }
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    }
}

#[cfg(test)]
#[path = "volume_binding_tests.rs"]
mod tests;
