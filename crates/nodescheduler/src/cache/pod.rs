//! `PodInfo` — what a scheduler needs to know about a pod, and nothing else.
//!
//! # Why this is a projection and not a `v1.Pod`
//!
//! Upstream caches whole API objects. A `v1.Pod` carries container commands,
//! env vars, volume mounts, per-container statuses, probe definitions,
//! managedFields — none of which any scheduling decision reads. On a cluster
//! with tens of thousands of pods that difference is the bulk of the
//! scheduler's resident memory, and it grows with the cluster, which is the
//! wrong direction for a project that targets ordinary multi-node clusters
//! rather than one-node edge boxes.
//!
//! So the watch layer projects once, on arrival, and everything downstream
//! sees only the fields below. The cost is that a field not projected here is
//! invisible to every plugin — adding a plugin that needs a new field means
//! adding it here first, deliberately, rather than discovering at runtime that
//! the data was silently absent.
//!
//! # `queued_at` is not `creationTimestamp`
//!
//! It is the time the pod entered the scheduling queue. A pod that fails a
//! cycle and is requeued keeps its **original** value, so `PrioritySort`'s
//! tie-break still favours it over pods that arrived later. Resetting it on
//! requeue starves exactly the pods that are having the hardest time getting
//! placed — a failure that only appears under contention and looks like
//! unfairness rather than a bug.

use k8s_openapi::api::core::v1::{
    Affinity, NodeSelector, Pod, PodSchedulingGate, Toleration, TopologySpreadConstraint,
};
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use std::collections::BTreeMap;

/// Scoring substitutes these for containers that declare no request, so a pod
/// asking for nothing does not appear free and pack infinitely onto one node.
///
/// **Scoring only — never filtering.** A pod with no requests genuinely fits
/// anywhere and must not be rejected for failing to fit a request it never
/// made. Mixing the two changes bin-packing in a way no test catches unless
/// it is written for exactly this, which is why `non_zero_requests()` is a
/// separate method rather than a flag on the main one.
pub const DEFAULT_MILLI_CPU_REQUEST: i64 = 100;
pub const DEFAULT_MEMORY_REQUEST: i64 = 200 * 1024 * 1024;

/// A resource vector. CPU in millicores, everything else in bytes or counts.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Resources {
    pub milli_cpu: i64,
    pub memory: i64,
    pub ephemeral_storage: i64,
    /// `hugepages-2Mi` and friends, keyed by the full resource name.
    pub hugepages: BTreeMap<String, i64>,
    /// `nvidia.com/gpu`, `example.com/foo` — anything not built in.
    pub extended: BTreeMap<String, i64>,
}

impl Resources {
    /// Element-wise sum, used to accumulate a node's committed total.
    pub fn add(&mut self, other: &Resources) {
        self.milli_cpu += other.milli_cpu;
        self.memory += other.memory;
        self.ephemeral_storage += other.ephemeral_storage;
        for (k, v) in &other.hugepages {
            *self.hugepages.entry(k.clone()).or_default() += v;
        }
        for (k, v) in &other.extended {
            *self.extended.entry(k.clone()).or_default() += v;
        }
    }

    /// Element-wise subtraction, for removing a pod from a node.
    ///
    /// Clamps at zero rather than going negative. A negative committed total
    /// is always a bookkeeping bug, and letting it go negative hides that bug
    /// by making the node look *more* free than empty — pods then schedule
    /// onto capacity that does not exist. Clamping keeps the error local and
    /// merely pessimistic.
    pub fn sub(&mut self, other: &Resources) {
        self.milli_cpu = (self.milli_cpu - other.milli_cpu).max(0);
        self.memory = (self.memory - other.memory).max(0);
        self.ephemeral_storage = (self.ephemeral_storage - other.ephemeral_storage).max(0);
        for (k, v) in &other.hugepages {
            let e = self.hugepages.entry(k.clone()).or_default();
            *e = (*e - v).max(0);
        }
        for (k, v) in &other.extended {
            let e = self.extended.entry(k.clone()).or_default();
            *e = (*e - v).max(0);
        }
    }

    /// Element-wise element lookup by Kubernetes resource name, so the fit
    /// check can iterate the requested names generically instead of
    /// special-casing four fields and forgetting extended resources.
    pub fn get(&self, name: &str) -> i64 {
        match name {
            "cpu" => self.milli_cpu,
            "memory" => self.memory,
            "ephemeral-storage" => self.ephemeral_storage,
            _ if name.starts_with("hugepages-") => {
                self.hugepages.get(name).copied().unwrap_or(0)
            }
            _ => self.extended.get(name).copied().unwrap_or(0),
        }
    }

    pub fn set(&mut self, name: &str, value: i64) {
        match name {
            "cpu" => self.milli_cpu = value,
            "memory" => self.memory = value,
            "ephemeral-storage" => self.ephemeral_storage = value,
            _ if name.starts_with("hugepages-") => {
                self.hugepages.insert(name.to_string(), value);
            }
            _ => {
                self.extended.insert(name.to_string(), value);
            }
        }
    }

    /// Every resource name carrying a non-zero value.
    pub fn names(&self) -> Vec<String> {
        let mut out = Vec::new();
        if self.milli_cpu != 0 {
            out.push("cpu".to_string());
        }
        if self.memory != 0 {
            out.push("memory".to_string());
        }
        if self.ephemeral_storage != 0 {
            out.push("ephemeral-storage".to_string());
        }
        out.extend(self.hugepages.iter().filter(|(_, v)| **v != 0).map(|(k, _)| k.clone()));
        out.extend(self.extended.iter().filter(|(_, v)| **v != 0).map(|(k, _)| k.clone()));
        out
    }

    /// Build from an API `resources.requests`/`allocatable` map.
    pub fn from_quantities(map: &BTreeMap<String, Quantity>) -> Self {
        let mut r = Resources::default();
        for (name, q) in map {
            let v = if name == "cpu" {
                parse_quantity_milli(&q.0)
            } else {
                parse_quantity(&q.0)
            };
            r.set(name, v);
        }
        r
    }

    /// Element-wise max, for folding init-container requests in.
    pub fn max_with(&mut self, other: &Resources) {
        self.milli_cpu = self.milli_cpu.max(other.milli_cpu);
        self.memory = self.memory.max(other.memory);
        self.ephemeral_storage = self.ephemeral_storage.max(other.ephemeral_storage);
        for (k, v) in &other.hugepages {
            let e = self.hugepages.entry(k.clone()).or_default();
            *e = (*e).max(*v);
        }
        for (k, v) in &other.extended {
            let e = self.extended.entry(k.clone()).or_default();
            *e = (*e).max(*v);
        }
    }
}

/// Parse a Kubernetes quantity to its base unit (bytes, or a plain count).
///
/// Kubernetes accepts binary suffixes (Ki/Mi/Gi/…), decimal SI suffixes
/// (k/M/G/…, where lowercase `k` is the SI one and there is no `K`), decimal
/// exponents (`1e3`), and the milli suffix `m`. This has to be right rather
/// than approximate — it feeds real filtering decisions, and a memory limit
/// parsed as a count instead of bytes silently makes a node look infinitely
/// large.
pub fn parse_quantity(s: &str) -> i64 {
    let s = s.trim();
    if s.is_empty() {
        return 0;
    }
    // Milli, the one suffix that divides rather than multiplies. Rounds up:
    // 1500m of a countable resource is 2 whole units, and rounding down would
    // let a pod claim less than it asked for.
    if let Some(num) = s.strip_suffix('m') {
        if let Ok(v) = num.parse::<f64>() {
            return (v / 1000.0).ceil() as i64;
        }
    }
    let binary = [("Ki", 1i64 << 10), ("Mi", 1 << 20), ("Gi", 1 << 30), ("Ti", 1 << 40), ("Pi", 1 << 50), ("Ei", 1 << 60)];
    for (suffix, mult) in binary {
        if let Some(num) = s.strip_suffix(suffix) {
            return num.trim().parse::<f64>().map(|v| (v * mult as f64) as i64).unwrap_or(0);
        }
    }
    let decimal = [("k", 1e3), ("M", 1e6), ("G", 1e9), ("T", 1e12), ("P", 1e15), ("E", 1e18)];
    for (suffix, mult) in decimal {
        if let Some(num) = s.strip_suffix(suffix) {
            return num.trim().parse::<f64>().map(|v| (v * mult) as i64).unwrap_or(0);
        }
    }
    // Bare number, possibly in exponent form (`1e3`). f64 handles both.
    s.parse::<f64>().map(|v| v as i64).unwrap_or(0)
}

/// Parse a quantity into **millis** — the representation CPU is stored in.
pub fn parse_quantity_milli(s: &str) -> i64 {
    let s = s.trim();
    if let Some(num) = s.strip_suffix('m') {
        return num.trim().parse::<f64>().map(|v| v as i64).unwrap_or(0);
    }
    if s.is_empty() {
        return 0;
    }
    s.parse::<f64>().map(|v| (v * 1000.0).round() as i64).unwrap_or(0)
}

/// A host port a pod has claimed.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct HostPort {
    pub protocol: String,
    /// Empty or `0.0.0.0` means "all addresses", which collides with every
    /// other claim on the same protocol and port.
    pub ip: String,
    pub port: i32,
}

impl HostPort {
    /// Whether two claims cannot coexist on one node.
    pub fn conflicts_with(&self, other: &HostPort) -> bool {
        if self.port != other.port || self.protocol != other.protocol {
            return false;
        }
        let wildcard = |ip: &str| ip.is_empty() || ip == "0.0.0.0" || ip == "::";
        wildcard(&self.ip) || wildcard(&other.ip) || self.ip == other.ip
    }
}

/// The scheduler's view of a pod.
#[derive(Clone, Debug, Default)]
pub struct PodInfo {
    pub namespace: String,
    pub name: String,
    pub uid: String,
    /// `None` until this pod is placed. The single field that decides whether
    /// this pod is the queue's business or the cache's.
    pub node_name: Option<String>,
    pub scheduler_name: String,
    pub priority: i32,
    pub preemption_policy: Option<String>,
    pub labels: BTreeMap<String, String>,
    pub node_selector: BTreeMap<String, String>,
    pub node_affinity: Option<Box<NodeSelector>>,
    pub preferred_node_affinity: Vec<PreferredTerm>,
    pub tolerations: Vec<Toleration>,
    pub topology_spread_constraints: Vec<TopologySpreadConstraint>,
    pub affinity: Option<Box<Affinity>>,
    pub scheduling_gates: Vec<PodSchedulingGate>,
    pub host_ports: Vec<HostPort>,
    /// Filtering requests: exactly what the pod asked for, nothing substituted.
    pub requests: Resources,
    /// Container images, for `ImageLocality`.
    pub images: Vec<String>,
    pub container_count: usize,
    /// PVC names this pod mounts, for the phase-4 volume plugins.
    pub pvc_names: Vec<String>,
    /// `spec.resourceClaims`, for DRA.
    pub resource_claim_names: Vec<String>,
    pub owner_references: Vec<OwnerRef>,
    /// When this pod entered the queue. Preserved across requeues — see the
    /// module header.
    pub queued_at: chrono::DateTime<chrono::Utc>,
    /// How many scheduling cycles this pod has already burned, for backoff.
    pub attempts: u32,
    /// Set by preemption; the node this pod has been promised.
    pub nominated_node_name: Option<String>,
}

/// A weighted preferred node-affinity term, flattened out of the API shape so
/// scoring does not re-walk `Option` chains per node.
#[derive(Clone, Debug)]
pub struct PreferredTerm {
    pub weight: i32,
    pub selector: k8s_openapi::api::core::v1::NodeSelectorTerm,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OwnerRef {
    pub kind: String,
    pub name: String,
    pub uid: String,
}

impl PodInfo {
    /// `namespace/name`, the key everything is logged and indexed by.
    pub fn key(&self) -> String {
        format!("{}/{}", self.namespace, self.name)
    }

    /// Requests with unspecified CPU and memory filled in.
    ///
    /// **Scoring only.** See the constants' doc comment.
    pub fn non_zero_requests(&self) -> Resources {
        let mut r = self.requests.clone();
        if r.milli_cpu == 0 {
            r.milli_cpu = DEFAULT_MILLI_CPU_REQUEST;
        }
        if r.memory == 0 {
            r.memory = DEFAULT_MEMORY_REQUEST;
        }
        r
    }

    /// Project an API pod.
    pub fn from_pod(pod: &Pod, queued_at: chrono::DateTime<chrono::Utc>) -> Self {
        let spec = pod.spec.clone().unwrap_or_default();
        let meta = &pod.metadata;

        let affinity = spec.affinity.clone().map(Box::new);
        let node_affinity = affinity
            .as_ref()
            .and_then(|a| a.node_affinity.clone())
            .and_then(|na| na.required_during_scheduling_ignored_during_execution)
            .map(Box::new);
        let preferred_node_affinity = affinity
            .as_ref()
            .and_then(|a| a.node_affinity.clone())
            .and_then(|na| na.preferred_during_scheduling_ignored_during_execution)
            .unwrap_or_default()
            .into_iter()
            .map(|t| PreferredTerm { weight: t.weight, selector: t.preference })
            .collect();

        let mut host_ports = Vec::new();
        let mut images = Vec::new();
        for c in spec.containers.iter().chain(spec.init_containers.iter().flatten()) {
            images.push(c.image.clone().unwrap_or_default());
            for p in c.ports.iter().flatten() {
                // hostPort 0 means "unset", not "port zero".
                let Some(hp) = p.host_port else { continue };
                if hp == 0 {
                    continue;
                }
                host_ports.push(HostPort {
                    protocol: p.protocol.clone().unwrap_or_else(|| "TCP".to_string()),
                    ip: p.host_ip.clone().unwrap_or_default(),
                    port: hp,
                });
            }
        }

        PodInfo {
            namespace: meta.namespace.clone().unwrap_or_default(),
            name: meta.name.clone().unwrap_or_default(),
            uid: meta.uid.clone().unwrap_or_default(),
            node_name: spec.node_name.clone().filter(|n| !n.is_empty()),
            scheduler_name: spec
                .scheduler_name
                .clone()
                .unwrap_or_else(|| "default-scheduler".to_string()),
            priority: spec.priority.unwrap_or(0),
            preemption_policy: spec.preemption_policy.clone(),
            labels: meta.labels.clone().unwrap_or_default(),
            node_selector: spec.node_selector.clone().unwrap_or_default(),
            node_affinity,
            preferred_node_affinity,
            tolerations: spec.tolerations.clone().unwrap_or_default(),
            topology_spread_constraints: spec
                .topology_spread_constraints
                .clone()
                .unwrap_or_default(),
            affinity,
            scheduling_gates: spec.scheduling_gates.clone().unwrap_or_default(),
            host_ports,
            requests: pod_requests(pod),
            images,
            container_count: spec.containers.len(),
            pvc_names: spec
                .volumes
                .iter()
                .flatten()
                .filter_map(|v| {
                    v.persistent_volume_claim.as_ref().map(|p| p.claim_name.clone())
                })
                .collect(),
            resource_claim_names: spec
                .resource_claims
                .iter()
                .flatten()
                .map(|c| c.name.clone())
                .collect(),
            owner_references: meta
                .owner_references
                .iter()
                .flatten()
                .map(|o| OwnerRef {
                    kind: o.kind.clone(),
                    name: o.name.clone(),
                    uid: o.uid.clone(),
                })
                .collect(),
            queued_at,
            attempts: 0,
            nominated_node_name: pod
                .status
                .as_ref()
                .and_then(|s| s.nominated_node_name.clone()),
        }
    }
}

/// A pod's effective resource request.
///
/// The rule is not "sum everything": init containers run to completion one at
/// a time *before* the regular containers start, so their peak demand is the
/// largest single one, not their sum — hence element-wise max, then max
/// against the regular containers' sum. Sidecars (init containers with
/// `restartPolicy: Always`) do run alongside the regular ones, so they count
/// toward the sum instead. `spec.overhead` is added on top; it is the runtime's
/// own cost and is real capacity the node cannot give to anything else.
pub fn pod_requests(pod: &Pod) -> Resources {
    let Some(spec) = pod.spec.as_ref() else {
        return Resources::default();
    };

    let mut sum = Resources::default();
    for c in &spec.containers {
        if let Some(req) = c.resources.as_ref().and_then(|r| r.requests.as_ref()) {
            sum.add(&Resources::from_quantities(req));
        }
    }

    let mut init_max = Resources::default();
    for c in spec.init_containers.iter().flatten() {
        let Some(req) = c.resources.as_ref().and_then(|r| r.requests.as_ref()) else {
            continue;
        };
        let r = Resources::from_quantities(req);
        if c.restart_policy.as_deref() == Some("Always") {
            sum.add(&r);
        } else {
            init_max.max_with(&r);
        }
    }

    sum.max_with(&init_max);

    if let Some(overhead) = spec.overhead.as_ref() {
        sum.add(&Resources::from_quantities(overhead));
    }
    sum
}

#[cfg(test)]
#[path = "pod_tests.rs"]
mod tests;
