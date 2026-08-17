//! `NodeInfo` — a node plus a running total of what has been committed to it.
//!
//! # Why the totals are incremental
//!
//! `requested` is not recomputed by walking the node's pods; it is adjusted by
//! [`NodeInfo::add_pod`] and [`NodeInfo::remove_pod`]. On a node running 110
//! pods, recomputing per scheduling cycle is 110 map merges to learn something
//! that changed by one pod — and the scheduling cycle runs once per pending
//! pod, so the waste compounds exactly when the cluster is busiest.
//!
//! The cost of that choice is that the totals are only as correct as the
//! add/remove pairing. Every mutation therefore goes through those two
//! methods, both of which bump [`NodeInfo::generation`]; nothing else touches
//! the fields they maintain. The `sub` used underneath clamps at zero rather
//! than going negative, so a missed pairing shows up as a node that looks
//! *fuller* than it is — pessimistic and self-correcting on the next resync,
//! rather than overcommitting real capacity.
//!
//! # The pre-filtered pod subsets
//!
//! `pods_with_affinity` and `pods_with_required_anti_affinity` exist so
//! `InterPodAffinity` can iterate a handful of pods instead of every pod on
//! every node. Anti-affinity with a `topologyKey` other than
//! `kubernetes.io/hostname` is the known-expensive case upstream documents as
//! a scalability caveat; without these subsets it is quadratic in cluster size.

use super::pod::{HostPort, PodInfo, Resources};
use k8s_openapi::api::core::v1::{Node, Taint};
use std::collections::BTreeMap;
use std::sync::Arc;

/// An image present on a node, with what `ImageLocality` needs to score it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ImageState {
    pub size_bytes: i64,
    /// Kept for fixture/source-shape compatibility. The authoritative
    /// cluster-wide count lives in `Snapshot::image_node_counts`, because a
    /// per-node projection cannot update every other Node cheaply.
    pub num_nodes: i64,
}

/// The scheduler's view of a node.
#[derive(Clone, Debug, Default)]
pub struct NodeInfo {
    /// Full object retained only when an HTTP extender is configured. See
    /// `PodInfo::api_object`; ordinary clusters pay no duplicate-object cost.
    pub api_object: Option<Box<Node>>,
    pub name: String,
    pub labels: BTreeMap<String, String>,
    pub taints: Vec<Taint>,
    /// Cordoned. `NodeUnschedulable` filters on this directly rather than
    /// relying on the node controller having mirrored it into a taint.
    pub unschedulable: bool,
    /// `(type, status)` for each condition. Only the status is kept — the
    /// heartbeat timestamps that dominate node update traffic are exactly
    /// what this projection exists to drop.
    pub conditions: BTreeMap<String, String>,

    pub allocatable: Resources,
    /// Sum of the real requests of every pod placed here.
    pub requested: Resources,
    /// Same, but with scoring's 100m/200Mi substitution applied. Kept
    /// alongside rather than derived because scoring reads it per node per
    /// cycle and the substitution is per *pod*, so it cannot be recovered
    /// from `requested` after summing.
    pub non_zero_requested: Resources,
    /// `status.allocatable["pods"]`, the pod-count ceiling.
    pub allocatable_pods: i64,

    pub used_ports: Vec<HostPort>,
    pub images: BTreeMap<String, ImageState>,

    pub pods: Vec<Arc<PodInfo>>,
    pub pods_with_affinity: Vec<Arc<PodInfo>>,
    pub pods_with_required_anti_affinity: Vec<Arc<PodInfo>>,

    /// Bumped on every mutation. The snapshot uses this to copy only what
    /// changed — see `snapshot.rs`.
    pub generation: u64,
}

impl NodeInfo {
    /// Project an API node. Does not touch the pod-derived fields, so this is
    /// safe to call on an update without losing the running totals.
    pub fn update_from_node(&mut self, node: &Node, generation: u64) {
        self.name = node.metadata.name.clone().unwrap_or_default();
        self.labels = node.metadata.labels.clone().unwrap_or_default();

        let spec = node.spec.clone().unwrap_or_default();
        self.taints = spec.taints.unwrap_or_default();
        self.unschedulable = spec.unschedulable.unwrap_or(false);

        if let Some(status) = node.status.as_ref() {
            if let Some(alloc) = status.allocatable.as_ref() {
                self.allocatable = Resources::from_quantities(alloc);
                self.allocatable_pods = alloc
                    .get("pods")
                    .map(|q| super::pod::parse_quantity(&q.0))
                    .unwrap_or(0);
            }
            self.conditions = status
                .conditions
                .iter()
                .flatten()
                .map(|c| (c.type_.clone(), c.status.clone()))
                .collect();
            self.images = status
                .images
                .iter()
                .flatten()
                .flat_map(|img| {
                    let size = img.size_bytes.unwrap_or(0);
                    img.names.iter().flatten().map(move |n| {
                        (n.clone(), ImageState { size_bytes: size, num_nodes: 1 })
                    })
                })
                .collect();
        }
        self.generation = generation;
    }

    /// Commit a pod's resources to this node.
    ///
    /// Idempotent by uid: re-adding a pod already present replaces it rather
    /// than double-counting. That matters because the same pod legitimately
    /// arrives twice — once assumed by our own scheduling cycle, then again
    /// when the informer delivers the real bound object — and double-counting
    /// there would make every node the scheduler placed onto look twice as
    /// full as it is.
    pub fn add_pod(&mut self, pod: Arc<PodInfo>, generation: u64) {
        if self.pods.iter().any(|p| p.uid == pod.uid) {
            self.remove_pod(&pod.uid, generation);
        }
        self.requested.add(&pod.requests);
        self.non_zero_requested.add(&pod.non_zero_requests());
        self.used_ports.extend(pod.host_ports.iter().cloned());

        if pod_has_affinity(&pod) {
            self.pods_with_affinity.push(pod.clone());
        }
        if pod_has_required_anti_affinity(&pod) {
            self.pods_with_required_anti_affinity.push(pod.clone());
        }
        self.pods.push(pod);
        self.generation = generation;
    }

    /// Release a pod's resources. Returns the pod if it was here.
    pub fn remove_pod(&mut self, uid: &str, generation: u64) -> Option<Arc<PodInfo>> {
        let idx = self.pods.iter().position(|p| p.uid == uid)?;
        let pod = self.pods.remove(idx);

        self.requested.sub(&pod.requests);
        self.non_zero_requested.sub(&pod.non_zero_requests());
        for hp in &pod.host_ports {
            if let Some(i) = self.used_ports.iter().position(|u| u == hp) {
                self.used_ports.remove(i);
            }
        }
        self.pods_with_affinity.retain(|p| p.uid != uid);
        self.pods_with_required_anti_affinity.retain(|p| p.uid != uid);

        self.generation = generation;
        Some(pod)
    }

    /// Whether a host-port claim would collide with something already here.
    pub fn port_conflicts(&self, want: &HostPort) -> bool {
        self.used_ports.iter().any(|u| u.conflicts_with(want))
    }

    /// Capacity left after what is already committed. Never negative.
    pub fn free(&self) -> Resources {
        let mut f = self.allocatable.clone();
        f.sub(&self.requested);
        f
    }

    pub fn pod_count(&self) -> i64 {
        self.pods.len() as i64
    }

    /// `Ready` condition is `True`. Not used for filtering — an unready node
    /// is normally excluded by its taints, which is upstream's mechanism too —
    /// but it is what the scheduler logs when explaining why nothing fit.
    pub fn is_ready(&self) -> bool {
        self.conditions.get("Ready").map(|s| s == "True").unwrap_or(false)
    }
}

fn pod_has_affinity(pod: &PodInfo) -> bool {
    pod.affinity
        .as_ref()
        .map(|a| a.pod_affinity.is_some() || a.pod_anti_affinity.is_some())
        .unwrap_or(false)
}

fn pod_has_required_anti_affinity(pod: &PodInfo) -> bool {
    pod.affinity
        .as_ref()
        .and_then(|a| a.pod_anti_affinity.as_ref())
        .and_then(|aa| aa.required_during_scheduling_ignored_during_execution.as_ref())
        .map(|terms| !terms.is_empty())
        .unwrap_or(false)
}

#[cfg(test)]
#[path = "node_tests.rs"]
mod tests;
