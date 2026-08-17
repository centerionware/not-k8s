//! The cache, and the per-cycle snapshot taken from it.
//!
//! # Why the snapshot is incremental
//!
//! A scheduling cycle must see a *stable* view of the cluster: filtering and
//! scoring run across every node, and a node changing halfway through would
//! let a pod be rejected by a state that no longer exists. So each cycle takes
//! a snapshot first.
//!
//! Doing that by cloning every node per cycle is the single biggest
//! scalability mistake available in a scheduler. The cycle runs once per
//! pending pod, so a 5000-node cluster draining a 10000-pod backlog would copy
//! 50,000,000 node structures to observe changes to a handful.
//!
//! Instead every [`NodeInfo`] carries a generation that increments on every
//! mutation, and the cache keeps nodes in most-recently-modified-first order.
//! [`Cache::update_snapshot`] walks from the head and **stops at the first
//! node whose generation the snapshot already has** — everything past that
//! point is, by construction, unchanged, and its `Arc` is reused. Three
//! changed nodes cost three clones regardless of cluster size.
//!
//! That correctness argument rests entirely on the MRU ordering being
//! maintained: a mutated node must move to the head, or the walk stops early
//! and silently misses it. `touch()` is the only place that ordering is
//! established, and every mutating method goes through it.
//!
//! # Node iteration order
//!
//! `node_tree` orders nodes round-robin across `topology.kubernetes.io/zone`
//! rather than by name. Combined with the cycle's `next_start_node_index`,
//! that makes the natural iteration order spread pods across failure domains
//! even before `PodTopologySpread` scores anything — which is what stops a
//! cluster whose node names sort by rack from packing one rack first.
//!
//! That round-robin order is itself only recomputed when it actually could
//! have changed — a node joining/leaving, or an existing node's zone label
//! changing. A pod binding or unbinding (by far the most common mutation,
//! and the reason the cache's generation bumps on nearly every cycle) moves
//! neither, so `update_snapshot` patches that node's entry in place via
//! `node_positions` instead of paying for a full `zone_round_robin` walk.
//! Skipping this would quietly undo the whole point of the incremental
//! design above: the node *contents* would stay O(changed), but the node
//! *order* would go back to O(cluster size) on nearly every cycle.

use super::dra::{RawDeviceClass, RawResourceClaim, RawResourceSlice};
use super::node::NodeInfo;
use super::pod::PodInfo;
use super::storage::{CsiDriverInfo, CsiNodeInfo, PvInfo, PvcInfo, StorageCapacityInfo, StorageClassInfo};
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

/// A stable, cheap-to-clone view of the cluster for one scheduling cycle.
#[derive(Clone, Debug, Default)]
pub struct Snapshot {
    /// Zone-round-robin order — see the module header.
    nodes: Vec<Arc<NodeInfo>>,
    /// name -> index into `nodes`, kept in step with it. Lets
    /// `update_snapshot` patch a changed node's entry in place — without
    /// recomputing the whole round-robin order — whenever the change didn't
    /// touch cluster membership or any node's zone. Only trustworthy right
    /// after `nodes` was itself rebuilt; see `update_snapshot`.
    node_positions: HashMap<String, usize>,
    by_name: HashMap<String, Arc<NodeInfo>>,
    /// Pre-filtered subsets, so `InterPodAffinity` does not walk every node to
    /// find the few that matter.
    pub nodes_with_pods_with_affinity: Vec<Arc<NodeInfo>>,
    pub nodes_with_pods_with_required_anti_affinity: Vec<Arc<NodeInfo>>,
    /// Cluster-wide image spread, maintained incrementally when Node status
    /// changes. `ImageLocality` must use every node in the snapshot here,
    /// not merely the feasible subset handed to `PreScore`.
    pub(crate) image_node_counts: Arc<HashMap<String, i64>>,
    /// The highest generation any node in this snapshot carries. The walk
    /// stops when it reaches a node at or below this.
    generation: u64,

    /// Namespace labels, for `InterPodAffinity`'s `namespaceSelector`.
    ///
    /// The only consumer, and the only reason a scheduler watches Namespaces
    /// at all. Without it a term using `namespaceSelector` cannot be
    /// evaluated, and the choice is between under-matching — silently
    /// disabling a rule the author wrote — and over-matching. Neither is
    /// parity; resolving it properly is.
    pub namespaces: Arc<HashMap<String, BTreeMap<String, String>>>,

    /// Label selectors of the workloads that own pods, for
    /// `PodTopologySpread`'s system default constraints.
    ///
    /// Upstream derives the default constraints' selector from the pod's
    /// owning Service/ReplicaSet/ReplicationController/StatefulSet, which is
    /// what these four watches exist for and their only consumer.
    pub workload_selectors: Vec<WorkloadSelector>,

    /// Phase 4's storage objects, copied wholesale — see the module header
    /// in `storage.rs` for why these are not tracked incrementally the way
    /// nodes are.
    pub pvs: Arc<HashMap<String, PvInfo>>,
    pub pvcs: Arc<HashMap<String, PvcInfo>>,
    pub storage_classes: Arc<HashMap<String, StorageClassInfo>>,
    /// Keyed by node name.
    pub csi_nodes: Arc<HashMap<String, CsiNodeInfo>>,
    /// Keyed by driver name.
    pub csi_drivers: Arc<HashMap<String, CsiDriverInfo>>,
    pub storage_capacities: Arc<Vec<StorageCapacityInfo>>,
    /// Keyed by VolumeAttachment object name.
    pub volume_attachments: Arc<HashMap<String, super::VolumeAttachmentInfo>>,

    /// Phase 5's DRA objects, copied wholesale for the same reason as
    /// Phase 4's storage objects — see `storage.rs`'s module header.
    /// Keyed `namespace/name`.
    pub resource_claims: Arc<HashMap<String, RawResourceClaim>>,
    /// Keyed by class name.
    pub device_classes: Arc<HashMap<String, RawDeviceClass>>,
    /// Keyed by slice object name.
    pub resource_slices: Arc<HashMap<String, RawResourceSlice>>,
}

/// One workload's selector, for deriving a pod's default spread constraints.
#[derive(Clone, Debug)]
pub struct WorkloadSelector {
    pub namespace: String,
    pub selector: k8s_openapi::apimachinery::pkg::apis::meta::v1::LabelSelector,
}

impl Snapshot {
    pub fn nodes(&self) -> &[Arc<NodeInfo>] {
        &self.nodes
    }

    pub fn node(&self, name: &str) -> Option<&Arc<NodeInfo>> {
        self.by_name.get(name)
    }

    pub fn num_nodes(&self) -> usize {
        self.nodes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// Every workload selector in this namespace that matches these labels.
    ///
    /// Upstream ANDs them: a pod owned by a ReplicaSet *and* selected by a
    /// Service spreads against the intersection, not against either alone.
    pub fn matching_workload_selectors(
        &self,
        namespace: &str,
        labels: &BTreeMap<String, String>,
    ) -> Vec<&WorkloadSelector> {
        self.workload_selectors
            .iter()
            .filter(|w| {
                w.namespace == namespace
                    && crate::framework::plugins::selector::matches_selector(
                        Some(&w.selector),
                        labels,
                    )
            })
            .collect()
    }

    /// How many nodes hold a given image, for `ImageLocality`'s spread ratio.
    pub fn nodes_with_image(&self, image: &str) -> i64 {
        self.nodes.iter().filter(|n| n.images.contains_key(image)).count() as i64
    }

    pub fn pv(&self, name: &str) -> Option<&super::storage::PvInfo> {
        self.pvs.get(name)
    }

    pub fn pvc(&self, namespace: &str, name: &str) -> Option<&super::storage::PvcInfo> {
        self.pvcs.get(&format!("{namespace}/{name}"))
    }

    pub fn storage_class(&self, name: &str) -> Option<&super::storage::StorageClassInfo> {
        self.storage_classes.get(name)
    }

    pub fn csi_node(&self, node_name: &str) -> Option<&super::storage::CsiNodeInfo> {
        self.csi_nodes.get(node_name)
    }

    pub fn csi_driver(&self, name: &str) -> Option<&super::storage::CsiDriverInfo> {
        self.csi_drivers.get(name)
    }

    pub fn resource_claim(&self, namespace: &str, name: &str) -> Option<&RawResourceClaim> {
        self.resource_claims.get(&format!("{namespace}/{name}"))
    }

    pub fn device_class(&self, name: &str) -> Option<&RawDeviceClass> {
        self.device_classes.get(name)
    }

    /// Every `ResourceSlice` that could supply a device to `node`: its own
    /// node-local slices, every `allNodes: true` slice, every
    /// `nodeSelector`-scoped slice this node's labels satisfy, and every
    /// `perDeviceNodeSelection: true` slice unconditionally — the latter
    /// defers node applicability to each individual device, which
    /// `dynamic_resources.rs`'s `candidates_for_node` resolves once it has
    /// the device in hand (see that function's doc comment).
    pub fn resource_slices_for_node<'a>(
        &'a self,
        node: &'a NodeInfo,
    ) -> impl Iterator<Item = &'a RawResourceSlice> + 'a {
        self.resource_slices.values().filter(move |s| {
            s.spec.node_name.as_deref() == Some(node.name.as_str())
                || s.spec.all_nodes == Some(true)
                || s.spec.per_device_node_selection == Some(true)
                || s.spec
                    .node_selector
                    .as_ref()
                    .is_some_and(|sel| crate::framework::plugins::node_affinity::matches_node_selector(sel, node))
        })
    }

    /// Every pod, cluster-wide, that references this PVC — for
    /// `ReadWriteOncePod`, which is a cluster-wide exclusivity rule, not a
    /// per-node one. Walking every node's pod list here rather than
    /// maintaining a dedicated index is the same trade `matching_workload_selectors`
    /// makes: this runs at most once per pod per scheduling cycle, not once
    /// per node, so it is linear in cluster pod count rather than quadratic.
    pub fn pods_using_pvc<'a>(
        &'a self,
        namespace: &str,
        pvc_name: &str,
    ) -> impl Iterator<Item = &'a Arc<PodInfo>> + 'a {
        let namespace = namespace.to_string();
        let pvc_name = pvc_name.to_string();
        self.nodes.iter().flat_map(|n| n.pods.iter()).filter(move |p| {
            p.namespace == namespace && p.pvc_names.iter().any(|n| *n == pvc_name)
        })
    }
}

/// The authoritative cluster state the snapshot is taken from.
#[derive(Debug, Default)]
pub struct Cache {
    nodes: HashMap<String, Arc<NodeInfo>>,
    /// Node names, most-recently-modified first. The snapshot walk depends on
    /// this being accurate; see the module header.
    mru: Vec<String>,
    /// Monotonic, bumped per mutation and stamped onto the node it touched.
    generation: u64,
    /// Which node each pod is committed to, so a pod can be removed without
    /// the caller having to remember where it went. Deleted pods routinely
    /// arrive with only a uid.
    pod_locations: HashMap<String, String>,

    /// Namespace labels and workload selectors. Unlike nodes and pods these
    /// change rarely and are small, so they are copied wholesale into each
    /// snapshot rather than tracked by generation — the same trade the PDB
    /// mirror makes, and for the same reason.
    namespaces: Arc<HashMap<String, BTreeMap<String, String>>>,
    workload_selectors: HashMap<String, WorkloadSelector>,

    /// Phase 4's storage objects. Same wholesale-copy treatment as
    /// `namespaces` — see `storage.rs`'s module header.
    pvs: Arc<HashMap<String, PvInfo>>,
    pvcs: Arc<HashMap<String, PvcInfo>>,
    storage_classes: Arc<HashMap<String, StorageClassInfo>>,
    csi_nodes: Arc<HashMap<String, CsiNodeInfo>>,
    csi_drivers: Arc<HashMap<String, CsiDriverInfo>>,
    storage_capacities: Arc<Vec<StorageCapacityInfo>>,
    volume_attachments: Arc<HashMap<String, super::VolumeAttachmentInfo>>,

    resource_claims: Arc<HashMap<String, RawResourceClaim>>,
    device_classes: Arc<HashMap<String, RawDeviceClass>>,
    resource_slices: Arc<HashMap<String, RawResourceSlice>>,
    /// Number of Nodes advertising each image name. Node updates are rare
    /// compared with Pod scheduling cycles, so maintaining this at the event
    /// boundary avoids rescanning the cluster for every Pod.
    image_node_counts: Arc<HashMap<String, i64>>,
}

impl Cache {
    pub fn new() -> Self {
        Self::default()
    }

    fn next_generation(&mut self) -> u64 {
        self.generation += 1;
        self.generation
    }

    /// Move a node to the front of the MRU list. Every mutation must call
    /// this, or the snapshot walk terminates before reaching it.
    fn touch(&mut self, name: &str) {
        if let Some(i) = self.mru.iter().position(|n| n == name) {
            self.mru.remove(i);
        }
        self.mru.insert(0, name.to_string());
    }

    /// Add or update a node, preserving the pods already committed to it.
    pub fn upsert_node(&mut self, node: &k8s_openapi::api::core::v1::Node) {
        self.upsert_node_with_api_object(node, false);
    }

    /// Add/update a node and optionally retain the full API object for HTTP
    /// extenders. The default cache path remains projection-only.
    pub fn upsert_node_with_api_object(
        &mut self,
        node: &k8s_openapi::api::core::v1::Node,
        preserve_api_object: bool,
    ) {
        let Some(name) = node.metadata.name.clone() else {
            return;
        };
        let gen = self.next_generation();
        let previous_images = self
            .nodes
            .get(&name)
            .map(|n| n.images.keys().cloned().collect::<Vec<_>>())
            .unwrap_or_default();
        let mut info = self
            .nodes
            .get(&name)
            .map(|n| (**n).clone())
            .unwrap_or_default();
        info.update_from_node(node, gen);
        let next_images = info.images.keys().cloned().collect::<Vec<_>>();
        if previous_images != next_images {
            let counts = Arc::make_mut(&mut self.image_node_counts);
            for image in previous_images {
                if let Some(count) = counts.get_mut(&image) {
                    *count -= 1;
                    if *count == 0 {
                        counts.remove(&image);
                    }
                }
            }
            for image in next_images {
                *counts.entry(image).or_default() += 1;
            }
        }
        info.api_object = preserve_api_object.then(|| Box::new(node.clone()));
        self.nodes.insert(name.clone(), Arc::new(info));
        self.touch(&name);
    }

    /// Remove a node. Its pods go with it — they are gone from the cluster's
    /// point of view, and the controller-manager will recreate whatever should
    /// come back elsewhere.
    pub fn remove_node(&mut self, name: &str) {
        if let Some(node) = self.nodes.remove(name) {
            let counts = Arc::make_mut(&mut self.image_node_counts);
            for image in node.images.keys() {
                if let Some(count) = counts.get_mut(image) {
                    *count -= 1;
                    if *count == 0 {
                        counts.remove(image);
                    }
                }
            }
            for pod in &node.pods {
                self.pod_locations.remove(&pod.uid);
            }
        }
        self.mru.retain(|n| n != name);
        self.generation += 1;
    }

    /// Commit a pod to the node named in it.
    ///
    /// A pod for a node the cache has never seen is dropped rather than
    /// buffered. That happens transiently at startup, when the pod watch's
    /// initial list can land before the node watch's — and the node watch's
    /// own initial list carries the pods again, so buffering would only risk
    /// double-counting to avoid a gap that closes itself.
    pub fn add_pod(&mut self, pod: Arc<PodInfo>) {
        let Some(node_name) = pod.node_name.clone() else {
            return;
        };
        // A pod that moved (it was force-deleted and recreated elsewhere under
        // the same uid) has to be released from wherever it used to be, or its
        // resources stay committed to a node that is no longer running it.
        if let Some(old) = self.pod_locations.get(&pod.uid).cloned() {
            if old != node_name {
                self.remove_pod_from(&old, &pod.uid);
            }
        }
        let gen = self.next_generation();
        let Some(existing) = self.nodes.get(&node_name) else {
            return;
        };
        let mut info = (**existing).clone();
        let uid = pod.uid.clone();
        info.add_pod(pod, gen);
        self.nodes.insert(node_name.clone(), Arc::new(info));
        self.pod_locations.insert(uid, node_name.clone());
        self.touch(&node_name);
    }

    /// Release a pod by uid, wherever it is.
    pub fn remove_pod(&mut self, uid: &str) {
        if let Some(node_name) = self.pod_locations.remove(uid) {
            self.remove_pod_from(&node_name, uid);
        }
    }

    fn remove_pod_from(&mut self, node_name: &str, uid: &str) {
        let gen = self.next_generation();
        let Some(existing) = self.nodes.get(node_name) else {
            return;
        };
        let mut info = (**existing).clone();
        info.remove_pod(uid, gen);
        self.nodes.insert(node_name.to_string(), Arc::new(info));
        self.touch(node_name);
    }

    pub fn upsert_namespace(&mut self, name: &str, labels: BTreeMap<String, String>) -> bool {
        let added = !self.namespaces.contains_key(name);
        Arc::make_mut(&mut self.namespaces).insert(name.to_string(), labels);
        self.generation += 1;
        added
    }

    pub fn remove_namespace(&mut self, name: &str) {
        Arc::make_mut(&mut self.namespaces).remove(name);
        self.generation += 1;
    }

    /// `key` identifies the workload uniquely across kinds — a ReplicaSet and
    /// a Service may share a name in one namespace, and collapsing them would
    /// drop one of their selectors.
    pub fn upsert_workload(&mut self, key: String, w: WorkloadSelector) {
        self.workload_selectors.insert(key, w);
        self.generation += 1;
    }

    pub fn remove_workload(&mut self, key: &str) {
        self.workload_selectors.remove(key);
        self.generation += 1;
    }

    pub fn upsert_pv(&mut self, name: String, pv: PvInfo) -> bool {
        let added = !self.pvs.contains_key(&name);
        Arc::make_mut(&mut self.pvs).insert(name, pv);
        self.generation += 1;
        added
    }

    pub fn remove_pv(&mut self, name: &str) {
        Arc::make_mut(&mut self.pvs).remove(name);
        self.generation += 1;
    }

    pub fn upsert_pvc(&mut self, key: String, pvc: PvcInfo) -> bool {
        let added = !self.pvcs.contains_key(&key);
        Arc::make_mut(&mut self.pvcs).insert(key, pvc);
        self.generation += 1;
        added
    }

    pub fn remove_pvc(&mut self, key: &str) {
        Arc::make_mut(&mut self.pvcs).remove(key);
        self.generation += 1;
    }

    pub fn upsert_storage_class(&mut self, name: String, sc: StorageClassInfo) -> bool {
        let added = !self.storage_classes.contains_key(&name);
        Arc::make_mut(&mut self.storage_classes).insert(name, sc);
        self.generation += 1;
        added
    }

    pub fn remove_storage_class(&mut self, name: &str) {
        Arc::make_mut(&mut self.storage_classes).remove(name);
        self.generation += 1;
    }

    pub fn upsert_csi_node(&mut self, node_name: String, info: CsiNodeInfo) -> bool {
        let added = !self.csi_nodes.contains_key(&node_name);
        Arc::make_mut(&mut self.csi_nodes).insert(node_name, info);
        self.generation += 1;
        added
    }

    pub fn remove_csi_node(&mut self, node_name: &str) {
        Arc::make_mut(&mut self.csi_nodes).remove(node_name);
        self.generation += 1;
    }

    pub fn upsert_csi_driver(&mut self, name: String, info: CsiDriverInfo) -> bool {
        let added = !self.csi_drivers.contains_key(&name);
        Arc::make_mut(&mut self.csi_drivers).insert(name, info);
        self.generation += 1;
        added
    }

    pub fn remove_csi_driver(&mut self, name: &str) {
        Arc::make_mut(&mut self.csi_drivers).remove(name);
        self.generation += 1;
    }

    /// Whole-list rebuild, same trade the PDB mirror makes: few objects,
    /// rarely change, and `VolumeBinding` reads the whole set per pod anyway.
    pub fn set_storage_capacities(&mut self, capacities: Vec<StorageCapacityInfo>) {
        self.storage_capacities = Arc::new(capacities);
        self.generation += 1;
    }

    pub fn upsert_volume_attachment(
        &mut self,
        name: String,
        attachment: super::VolumeAttachmentInfo,
    ) -> bool {
        let added = !self.volume_attachments.contains_key(&name);
        Arc::make_mut(&mut self.volume_attachments).insert(name, attachment);
        self.generation += 1;
        added
    }

    pub fn remove_volume_attachment(&mut self, name: &str) {
        Arc::make_mut(&mut self.volume_attachments).remove(name);
        self.generation += 1;
    }

    pub fn upsert_resource_claim(&mut self, key: String, claim: RawResourceClaim) -> bool {
        let added = !self.resource_claims.contains_key(&key);
        Arc::make_mut(&mut self.resource_claims).insert(key, claim);
        self.generation += 1;
        added
    }

    pub fn remove_resource_claim(&mut self, key: &str) {
        Arc::make_mut(&mut self.resource_claims).remove(key);
        self.generation += 1;
    }

    pub fn upsert_device_class(&mut self, name: String, class: RawDeviceClass) -> bool {
        let added = !self.device_classes.contains_key(&name);
        Arc::make_mut(&mut self.device_classes).insert(name, class);
        self.generation += 1;
        added
    }

    pub fn remove_device_class(&mut self, name: &str) {
        Arc::make_mut(&mut self.device_classes).remove(name);
        self.generation += 1;
    }

    pub fn upsert_resource_slice(&mut self, name: String, slice: RawResourceSlice) -> bool {
        let added = !self.resource_slices.contains_key(&name);
        Arc::make_mut(&mut self.resource_slices).insert(name, slice);
        self.generation += 1;
        added
    }

    pub fn remove_resource_slice(&mut self, name: &str) {
        Arc::make_mut(&mut self.resource_slices).remove(name);
        self.generation += 1;
    }

    /// Which node a pod is committed to, if any.
    pub fn pod_node(&self, uid: &str) -> Option<&str> {
        self.pod_locations.get(uid).map(String::as_str)
    }

    pub fn num_nodes(&self) -> usize {
        self.nodes.len()
    }

    /// Refresh `snapshot` in place, copying only what changed.
    ///
    /// See the module header for why this is the shape it is.
    pub fn update_snapshot(&self, snapshot: &mut Snapshot) {
        let mut changed: Vec<Arc<NodeInfo>> = Vec::new();
        for name in &self.mru {
            let Some(node) = self.nodes.get(name) else {
                continue;
            };
            // The walk's whole point: past here, nothing has moved since the
            // snapshot was last taken.
            if node.generation <= snapshot.generation {
                break;
            }
            changed.push(node.clone());
        }

        // A node removed from the cache leaves no trace in the MRU walk — it
        // is simply absent — so a snapshot is only correct if it also drops
        // nodes the cache no longer has. Cheap: one lookup per retained node.
        let dropped_any = snapshot.by_name.keys().any(|n| !self.nodes.contains_key(n));

        // A namespace or workload change moves the cache generation without
        // touching any node, so the node walk finds nothing. Returning early
        // there would leave the snapshot holding stale namespace labels
        // forever — the sort of staleness that surfaces as an affinity rule
        // that stopped applying and nothing to explain why.
        let metadata_stale = snapshot.generation != self.generation;
        if changed.is_empty() && !dropped_any && !metadata_stale {
            return;
        }

        // Whether the round-robin order can only have shifted (a node
        // joining/leaving, or an existing node's zone changing) — computed
        // against `snapshot.by_name` *before* this walk's changes are
        // applied to it, below. See the module header for why this matters.
        const ZONE_LABEL: &str = "topology.kubernetes.io/zone";
        let reorder_needed = dropped_any
            || changed.iter().any(|node| match snapshot.by_name.get(&node.name) {
                None => true, // newly joined the cluster
                Some(prev) => prev.labels.get(ZONE_LABEL) != node.labels.get(ZONE_LABEL),
            });

        for node in &changed {
            snapshot.by_name.insert(node.name.clone(), node.clone());
        }
        if dropped_any {
            snapshot.by_name.retain(|name, _| self.nodes.contains_key(name));
        }

        snapshot.generation = self.generation;
        snapshot.namespaces = self.namespaces.clone();
        snapshot.workload_selectors = self.workload_selectors.values().cloned().collect();
        snapshot.pvs = self.pvs.clone();
        snapshot.pvcs = self.pvcs.clone();
        snapshot.storage_classes = self.storage_classes.clone();
        snapshot.csi_nodes = self.csi_nodes.clone();
        snapshot.csi_drivers = self.csi_drivers.clone();
        snapshot.storage_capacities = self.storage_capacities.clone();
        snapshot.volume_attachments = self.volume_attachments.clone();
        snapshot.resource_claims = self.resource_claims.clone();
        snapshot.device_classes = self.device_classes.clone();
        snapshot.resource_slices = self.resource_slices.clone();
        snapshot.image_node_counts = self.image_node_counts.clone();
        if reorder_needed {
            snapshot.nodes = zone_round_robin(&snapshot.by_name);
            snapshot.node_positions = snapshot
                .nodes
                .iter()
                .enumerate()
                .map(|(i, n)| (n.name.clone(), i))
                .collect();
        } else {
            // Membership and every zone assignment are unchanged, so the
            // order stays valid — just refresh the entries the walk found,
            // in place, at the positions `node_positions` already knows.
            for node in &changed {
                if let Some(&i) = snapshot.node_positions.get(&node.name) {
                    snapshot.nodes[i] = node.clone();
                }
            }
        }
        snapshot.nodes_with_pods_with_affinity = snapshot
            .nodes
            .iter()
            .filter(|n| !n.pods_with_affinity.is_empty())
            .cloned()
            .collect();
        snapshot.nodes_with_pods_with_required_anti_affinity = snapshot
            .nodes
            .iter()
            .filter(|n| !n.pods_with_required_anti_affinity.is_empty())
            .cloned()
            .collect();
    }

    /// A fresh snapshot, for the first cycle.
    pub fn snapshot(&self) -> Snapshot {
        let mut s = Snapshot::default();
        // generation 0 means "has nothing", so the walk copies everything.
        self.update_snapshot(&mut s);
        s
    }
}

/// Order nodes so that consecutive entries come from different zones.
///
/// Nodes with no zone label form their own group rather than being dropped or
/// bunched at the end — a cluster with no zone labels at all must still get a
/// stable, complete ordering, and it is the common case on bare metal.
fn zone_round_robin(by_name: &HashMap<String, Arc<NodeInfo>>) -> Vec<Arc<NodeInfo>> {
    const ZONE_LABEL: &str = "topology.kubernetes.io/zone";

    let mut zones: BTreeMap<&str, Vec<Arc<NodeInfo>>> = BTreeMap::new();
    let mut names: Vec<&String> = by_name.keys().collect();
    names.sort_unstable();
    for name in names {
        let node = &by_name[name];
        let zone = node.labels.get(ZONE_LABEL).map(String::as_str).unwrap_or("");
        zones.entry(zone).or_default().push(node.clone());
    }

    let mut out = Vec::with_capacity(by_name.len());
    let mut buckets: Vec<Vec<Arc<NodeInfo>>> = zones.into_values().collect();
    let mut i = 0;
    while out.len() < by_name.len() {
        let mut progressed = false;
        for bucket in buckets.iter_mut() {
            if let Some(node) = bucket.get(i) {
                out.push(node.clone());
                progressed = true;
            }
        }
        if !progressed {
            break;
        }
        i += 1;
    }
    out
}

#[cfg(test)]
#[path = "snapshot_tests.rs"]
mod tests;
