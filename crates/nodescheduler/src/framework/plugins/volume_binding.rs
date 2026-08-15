//! `VolumeBinding` — the plugin that actually gets a pod's unbound
//! `PersistentVolumeClaim`s bound, and checks a bound one's node constraint.
//!
//! # Static PVs, and the race two pods claiming one has
//!
//! An unbound PVC is matched against an already-existing, unclaimed
//! `PersistentVolume` first — an admin provisions PVs by hand, or a PVC
//! carries a `selector` — exactly the same priority order upstream's real
//! `findMatchingVolumes` uses: only when nothing static matches does dynamic
//! provisioning (below) get considered at all. A static match still has to
//! satisfy `PersistentVolume.spec.nodeAffinity` for whichever node ends up
//! winning, same as a bound PVC's PV does.
//!
//! Two pods whose PVCs could both claim the *same* free static PV is a real
//! scarce-resource race, the same shape `DynamicResources`' device
//! assume cache exists for (see that module's own doc comment) — `Reserve`
//! tentatively marks the PV it picked as claimed, `Unreserve`/`PostBind`
//! release the mark, and `PreFilter`'s candidate scan excludes both this
//! in-memory mark and every PV a real `claimRef` already names.
//!
//! What is implemented, faithfully:
//!
//!   * a static match (above), including the "pre-bound" case where
//!     `PersistentVolumeClaim.spec.volumeName` already names a specific PV —
//!     that PV is the only candidate considered, matching upstream;
//!   * failing a static match, an unbound PVC whose `StorageClass` is
//!     `Immediate` (or has none) blocks the pod outright — the PV
//!     controller, not this scheduler, is who binds it, and nothing about
//!     *which node* changes that;
//!   * failing a static match, an unbound PVC whose `StorageClass` is
//!     `WaitForFirstConsumer` is checked against that class's
//!     `allowedTopologies` and, when the driver opts into capacity
//!     tracking, `CSIStorageCapacity`;
//!   * `PreBind` writes either `PersistentVolumeClaim.spec.volumeName` (a
//!     static claim — the built-in PV binder controller completes the actual
//!     bind, including `PersistentVolume.spec.claimRef`, from that alone,
//!     the same controller this project's stripped control plane already
//!     runs) or the `volume.kubernetes.io/selected-node` annotation (dynamic
//!     provisioning, telling the external provisioner which node to
//!     provision for), then polls for `status.phase == "Bound"` either way —
//!     the one genuinely blocking wait in this whole design (see
//!     docs/SCHEDULER.md, invariant 4), which is exactly why it runs in the
//!     binding cycle and not the scheduling loop;
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
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub const NAME: &str = "VolumeBinding";

const POLL_INTERVAL: Duration = Duration::from_secs(2);

/// The annotation an external provisioner watches to learn which node it is
/// provisioning for. Part of the PVC API contract, not invented here.
const SELECTED_NODE_ANNOTATION: &str = "volume.kubernetes.io/selected-node";

/// One static candidate — a PV name plus the topology constraint choosing it
/// would impose, before it's resolved per node.
struct StaticCandidate {
    pv_name: String,
    node_affinity: Option<Box<NodeSelector>>,
}

/// What a candidate node has to satisfy for one of the pod's PVCs.
enum PvcConstraint {
    /// Already bound. `None` when the PV carries no `nodeAffinity` at all —
    /// which is the common case for a PV that predates topology-aware
    /// provisioning, or one whose zone is expressed as a label instead (see
    /// `volume_zone.rs`).
    Bound(Option<Box<NodeSelector>>),
    /// Unbound, but at least one already-existing, unclaimed PV matches —
    /// see the module header's "Static PVs" section. `by_node` maps each
    /// node that has a usable candidate to the PV name `Filter` accepted it
    /// for — resolved once in `PreFilter` (the same shape
    /// `DynamicResources`'s `ClaimPlan::Allocate` uses, and for the same
    /// reason: `ReservePlugin::reserve` gets only a node *name*, not the
    /// `NodeInfo` a `nodeAffinity` check needs, so the check cannot be
    /// deferred to Reserve time the way `Bound`/`Delayed` defer theirs to
    /// `Filter`). Can be empty — every candidate PV existed, but none of
    /// their `nodeAffinity`s matched any node in this snapshot — in which
    /// case every node is correctly `Unschedulable`, the same as an empty
    /// `by_node` in `DynamicResources`'s `ClaimPlan::Allocate`.
    Static { pvc_key: String, by_node: HashMap<String, String> },
    /// Unbound, `WaitForFirstConsumer`. Empty `allowed_topologies` means no
    /// restriction. `capacity` is `None` when the driver does not opt into
    /// capacity tracking (`CSIDriver.spec.storageCapacity`) — in that case
    /// the node is assumed to have room, the same as upstream.
    Delayed { allowed_topologies: Vec<TopologySelectorTerm>, capacity: Option<Vec<StorageCapacityInfo>>, requested_bytes: i64 },
}

struct WantedPvcs(Vec<PvcConstraint>);

/// `Clone`, deliberately: like `DynamicResources`, this plugin's assume
/// cache has to be the *same* instance everywhere it appears in the
/// `Registry` (`PreFilter` reads it, `Reserve` writes it,
/// `Unreserve`/`PostBind` clear it, `PreBind` reads it again) —
/// `default_registry` constructs one instance and clones it into every vec
/// it belongs to.
#[derive(Clone)]
pub struct VolumeBinding {
    client: kube::Client,
    bind_timeout: Duration,
    assumed_pvs: Arc<Mutex<HashSet<String>>>,
    /// pod uid -> (pvc name -> pv name), for `PreBind`/`Unreserve`/`PostBind`
    /// to find what `Reserve` picked for this pod, none of which see
    /// `CycleState`.
    assumed_by_pod: Arc<Mutex<HashMap<String, HashMap<String, String>>>>,
}

impl VolumeBinding {
    pub fn new(client: kube::Client, bind_timeout: Duration) -> Self {
        Self {
            client,
            bind_timeout,
            assumed_pvs: Arc::new(Mutex::new(HashSet::new())),
            assumed_by_pod: Arc::new(Mutex::new(HashMap::new())),
        }
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

/// Whether an already-existing `pv` could satisfy `pvc` — everything except
/// the node-dependent part (`nodeAffinity`, checked separately per node in
/// `Filter`/`Reserve`). Matches upstream's own `FindMatchingVolumes`
/// criteria: same `storageClassName` (both compared as plain strings, empty
/// for "none"), a claimRef that names nobody else, every requested access
/// mode present, enough capacity, and the PVC's own `selector` (if any)
/// satisfied by the PV's labels.
fn static_matches(pvc: &crate::cache::storage::PvcInfo, pv: &crate::cache::storage::PvInfo) -> bool {
    let claimed_by_someone_else =
        pv.claim_ref.as_ref().is_some_and(|(ns, name)| ns != &pvc.namespace || name != &pvc.name);
    if claimed_by_someone_else {
        return false;
    }
    // A PV already pre-bound to this exact PVC (claim_ref names it) is
    // usable regardless of phase — that's the whole "pre-bound" case. A PV
    // with no claimant yet must be Available: Released/Failed/Pending would
    // either never actually complete a bind or hand the pod storage the PV
    // controller itself no longer considers fit for reuse.
    let pre_bound_to_us = pv.claim_ref.as_ref().is_some_and(|(ns, name)| ns == &pvc.namespace && name == &pvc.name);
    if !pre_bound_to_us && pv.phase != "Available" {
        return false;
    }
    if pv.volume_mode != pvc.volume_mode {
        return false;
    }
    if pv.storage_class_name != pvc.storage_class_name.as_deref().unwrap_or("") {
        return false;
    }
    if !pvc.requested_access_modes.iter().all(|m| pv.access_modes.iter().any(|a| a == m)) {
        return false;
    }
    if pv.capacity_bytes < pvc.requested_bytes {
        return false;
    }
    if let Some(sel) = &pvc.selector {
        if !crate::framework::plugins::selector::matches_selector(Some(sel), &pv.labels) {
            return false;
        }
    }
    true
}

/// Every already-existing PV `pvc` could statically bind to, excluding
/// whatever another in-flight cycle has already tentatively claimed (`excluded`
/// — see the module header's assume-cache section). A PVC that already names
/// a specific PV (`spec.volumeName` — the "pre-bound" case, set by an admin
/// or the user directly rather than by this plugin) considers *only* that
/// PV, matching upstream: once a claim has committed to a specific volume by
/// name, nothing else is a valid substitute.
fn find_static_candidates(pvc: &crate::cache::storage::PvcInfo, snapshot: &Snapshot, excluded: &HashSet<String>) -> Vec<StaticCandidate> {
    let to_candidate = |pv: &crate::cache::storage::PvInfo| StaticCandidate {
        pv_name: pv.name.clone(),
        node_affinity: pv.node_affinity.clone(),
    };
    if let Some(vn) = &pvc.volume_name {
        return match snapshot.pv(vn) {
            Some(pv) if !excluded.contains(&pv.name) && static_matches(pvc, pv) => vec![to_candidate(pv)],
            _ => Vec::new(),
        };
    }
    snapshot
        .pvs
        .values()
        .filter(|pv| !excluded.contains(&pv.name) && static_matches(pvc, pv))
        .map(to_candidate)
        .collect()
}

/// Resolve `candidates` to the one PV each node in `snapshot` could actually
/// use (its `nodeAffinity`, if any, satisfied by that node's labels) — see
/// `PvcConstraint::Static`'s doc comment for why this has to happen once
/// here rather than lazily in `Filter`/`Reserve`. The first satisfying
/// candidate wins per node; which one is arbitrary among ties, the same way
/// `allocate_on_node`'s device picks are.
fn resolve_by_node(candidates: &[StaticCandidate], snapshot: &Snapshot) -> HashMap<String, String> {
    let mut by_node = HashMap::new();
    for node in snapshot.nodes() {
        if let Some(c) = candidates.iter().find(|c| {
            c.node_affinity
                .as_ref()
                .is_none_or(|sel| crate::framework::plugins::node_affinity::matches_node_selector(sel, node))
        }) {
            by_node.insert(node.name.clone(), c.pv_name.clone());
        }
    }
    by_node
}

impl PreFilterPlugin for VolumeBinding {
    fn pre_filter(
        &self,
        state: &mut CycleState,
        pod: &PodInfo,
        snapshot: &Snapshot,
    ) -> (Status, Option<Vec<String>>) {
        let excluded = self.assumed_pvs.lock().unwrap().clone();
        pre_filter_impl(state, pod, snapshot, &excluded)
    }
}

/// The pure half of `PreFilter` — no `&self` beyond the assume-cache
/// snapshot the trait impl above takes under lock and hands in as a plain
/// `&HashSet`, mirroring `dynamic_resources.rs`'s `pre_filter_impl` split;
/// every case below is tested directly against it.
fn pre_filter_impl(
    state: &mut CycleState,
    pod: &PodInfo,
    snapshot: &Snapshot,
    excluded_pvs: &HashSet<String>,
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

            // Static match first, unconditionally — upstream's own priority
            // order (see the module header). Only when nothing existing
            // matches does storage-class-driven dynamic provisioning get
            // considered at all.
            let candidates = find_static_candidates(pvc, snapshot, excluded_pvs);
            if !candidates.is_empty() {
                let by_node = resolve_by_node(&candidates, snapshot);
                wanted.push(PvcConstraint::Static { pvc_key: pvc.key(), by_node });
                continue;
            }
            if pvc.volume_name.is_some() {
                // Pre-bound to a specific PV that doesn't (yet, or ever)
                // actually satisfy this claim — never a case for dynamic
                // provisioning to paper over, since the claim has already
                // committed to that one volume.
                return (
                    Status::unschedulable(NAME, "persistentvolumeclaim names a PersistentVolume that is not ready or does not match it"),
                    None,
                );
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
                PvcConstraint::Static { by_node, .. } => {
                    if !by_node.contains_key(&node.name) {
                        return Status::unschedulable(NAME, "node(s) had no matching PersistentVolume for this node's topology");
                    }
                }
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

/// The pure half of `Reserve`: which PV (if any) to claim per `Static`
/// constraint, for the node that won. `Err` means a plugin bug (Filter
/// already checked this node has a usable candidate), not a scheduling
/// outcome. No `&self`, so every case is tested directly against it.
fn reserve_impl(wanted: &WantedPvcs, node: &str) -> Result<HashMap<String, String>, Status> {
    let mut picked = HashMap::new();
    for constraint in &wanted.0 {
        let PvcConstraint::Static { pvc_key, by_node } = constraint else { continue };
        let Some(pv_name) = by_node.get(node) else {
            return Err(Status::error(NAME, format!("internal: Filter approved node {node} with no usable static PV for {pvc_key}")));
        };
        picked.insert(pvc_key.clone(), pv_name.clone());
    }
    Ok(picked)
}

impl crate::framework::ReservePlugin for VolumeBinding {
    /// Pick one qualifying static candidate per `Static` constraint and mark
    /// it claimed — see the module header's "race two pods claiming one
    /// has" section. `Delayed`/`Bound` need no reservation: dynamic
    /// provisioning always makes a *new* volume (no shared pool to
    /// double-book), and a `Bound` PVC's PV is already spoken for by it.
    fn reserve(&self, state: &mut CycleState, pod: &PodInfo, node: &str) -> Status {
        let Some(wanted) = state.read::<WantedPvcs>(NAME) else {
            return Status::success();
        };

        let picked = match reserve_impl(&wanted, node) {
            Ok(picked) => picked,
            Err(status) => return status,
        };

        if !picked.is_empty() {
            self.assumed_pvs.lock().unwrap().extend(picked.values().cloned());
            self.assumed_by_pod.lock().unwrap().insert(pod.uid.clone(), picked);
        }
        Status::success()
    }

    /// Must not fail and must be idempotent — see the trait's own doc
    /// comment. A uid with nothing assumed is simply a no-op.
    fn unreserve(&self, _state: &mut CycleState, pod: &PodInfo, _node: &str) {
        self.release(&pod.uid);
    }
}

impl VolumeBinding {
    /// Drop this pod's tentative static-PV claims — called on any failure
    /// after `Reserve` (`Unreserve`) and on a successful bind (`PostBind`,
    /// once the real write has already landed, so nothing is lost by
    /// forgetting the local mark early — same reasoning
    /// `DynamicResources::release` documents).
    fn release(&self, pod_uid: &str) {
        if let Some(picked) = self.assumed_by_pod.lock().unwrap().remove(pod_uid) {
            let mut assumed = self.assumed_pvs.lock().unwrap();
            for pv_name in picked.values() {
                assumed.remove(pv_name);
            }
        }
    }
}

#[async_trait::async_trait]
impl crate::framework::PreBindPlugin for VolumeBinding {
    /// The one genuinely blocking wait in the whole design — see the module
    /// header and docs/SCHEDULER.md invariant 4. It runs on this pod's own
    /// binding-cycle task, so a slow provisioner stalls only this pod.
    async fn pre_bind(&self, pod: &PodInfo, node: &str) -> Status {
        let assumed_static = self.assumed_by_pod.lock().unwrap().get(&pod.uid).cloned().unwrap_or_default();
        for pvc_name in &pod.pvc_names {
            let key = format!("{}/{}", pod.namespace, pvc_name);
            let result = match assumed_static.get(&key) {
                Some(pv_name) => self.bind_static_pvc(pod, pvc_name, pv_name).await,
                None => self.bind_one_pvc(pod, pvc_name, node).await,
            };
            if let Err(status) = result {
                return status;
            }
        }
        Status::success()
    }
}

#[async_trait::async_trait]
impl crate::framework::PostBindPlugin for VolumeBinding {
    /// The apiserver write already landed in `PreBind` — this only clears
    /// the local tentative mark. See `ReservePlugin::unreserve`'s doc
    /// comment for why that is safe to do immediately.
    async fn post_bind(&self, pod: &PodInfo, _node: &str) {
        self.release(&pod.uid);
    }
}

impl VolumeBinding {
    /// A static claim: point the PVC at the specific PV `Reserve` picked and
    /// poll for `Bound`. Setting `spec.volumeName` is the whole write this
    /// side needs — the built-in PV binder controller (part of this
    /// project's stripped control plane, same as upstream's) observes a PVC
    /// naming a specific, matching, unclaimed PV and completes the bind
    /// itself, including `PersistentVolume.spec.claimRef`.
    async fn bind_static_pvc(&self, pod: &PodInfo, pvc_name: &str, pv_name: &str) -> Result<(), Status> {
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

        let already_named = current.spec.as_ref().and_then(|s| s.volume_name.as_deref()) == Some(pv_name);
        if !already_named {
            let patch = serde_json::json!({ "spec": { "volumeName": pv_name } });
            api.patch(
                pvc_name,
                &PatchParams { field_manager: Some("nodescheduler".to_string()), ..Default::default() },
                &Patch::Merge(patch),
            )
            .await
            .map_err(|e| Status::error(NAME, format!("claiming persistentvolume {pv_name:?} for persistentvolumeclaim {pvc_name:?}: {e}")))?;
        }

        self.poll_until_bound(&api, pvc_name).await
    }

    async fn poll_until_bound(
        &self,
        api: &kube::api::Api<k8s_openapi::api::core::v1::PersistentVolumeClaim>,
        pvc_name: &str,
    ) -> Result<(), Status> {
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
                    format!("persistentvolumeclaim {pvc_name:?} did not bind within {}s", self.bind_timeout.as_secs()),
                ));
            }
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    }

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

        self.poll_until_bound(&api, pvc_name).await
    }
}

#[cfg(test)]
#[path = "volume_binding_tests.rs"]
mod tests;
