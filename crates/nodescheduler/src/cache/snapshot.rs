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

use super::node::NodeInfo;
use super::pod::PodInfo;
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

/// A stable, cheap-to-clone view of the cluster for one scheduling cycle.
#[derive(Clone, Debug, Default)]
pub struct Snapshot {
    /// Zone-round-robin order — see the module header.
    nodes: Vec<Arc<NodeInfo>>,
    by_name: HashMap<String, Arc<NodeInfo>>,
    /// Pre-filtered subsets, so `InterPodAffinity` does not walk every node to
    /// find the few that matter.
    pub nodes_with_pods_with_affinity: Vec<Arc<NodeInfo>>,
    pub nodes_with_pods_with_required_anti_affinity: Vec<Arc<NodeInfo>>,
    /// The highest generation any node in this snapshot carries. The walk
    /// stops when it reaches a node at or below this.
    generation: u64,
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

    /// How many nodes hold a given image, for `ImageLocality`'s spread ratio.
    pub fn nodes_with_image(&self, image: &str) -> i64 {
        self.nodes.iter().filter(|n| n.images.contains_key(image)).count() as i64
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
        let Some(name) = node.metadata.name.clone() else {
            return;
        };
        let gen = self.next_generation();
        let mut info = self
            .nodes
            .get(&name)
            .map(|n| (**n).clone())
            .unwrap_or_default();
        info.update_from_node(node, gen);
        self.nodes.insert(name.clone(), Arc::new(info));
        self.touch(&name);
    }

    /// Remove a node. Its pods go with it — they are gone from the cluster's
    /// point of view, and the controller-manager will recreate whatever should
    /// come back elsewhere.
    pub fn remove_node(&mut self, name: &str) {
        if let Some(node) = self.nodes.remove(name) {
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

        if changed.is_empty() && !dropped_any {
            return;
        }

        for node in &changed {
            snapshot.by_name.insert(node.name.clone(), node.clone());
        }
        if dropped_any {
            snapshot.by_name.retain(|name, _| self.nodes.contains_key(name));
        }

        snapshot.generation = self.generation;
        snapshot.nodes = zone_round_robin(&snapshot.by_name);
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
