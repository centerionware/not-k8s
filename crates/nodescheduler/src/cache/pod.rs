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
    /// Saturating, not `+=`: a legal-but-absurd quantity like `cpu: "1E"`
    /// (accepted by the apiserver) parses to a value near `i64::MAX`, and
    /// plain addition on that panics in debug and wraps in release — a
    /// wrapped total goes negative, making the node look emptier than
    /// empty, the exact failure `sub`'s own clamp below exists to prevent.
    pub fn add(&mut self, other: &Resources) {
        self.milli_cpu = self.milli_cpu.saturating_add(other.milli_cpu);
        self.memory = self.memory.saturating_add(other.memory);
        self.ephemeral_storage = self.ephemeral_storage.saturating_add(other.ephemeral_storage);
        for (k, v) in &other.hugepages {
            let e = self.hugepages.entry(k.clone()).or_default();
            *e = e.saturating_add(*v);
        }
        for (k, v) in &other.extended {
            let e = self.extended.entry(k.clone()).or_default();
            *e = e.saturating_add(*v);
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

/// Parse a Kubernetes quantity into a plain number of base units.
///
/// # The suffix set is not optional, and the apiserver will use it
///
/// A quantity is `<signedNumber><suffix>`, where the suffix is a binary one
/// (`Ki`/`Mi`/`Gi`/`Ti`/`Pi`/`Ei`), a decimal SI one (`m`/`k`/`M`/`G`/`T`/
/// `P`/`E` — note lowercase `k`, there is no `K`), or a decimal exponent
/// (`e3`/`E3`).
///
/// Handling only the ones that *look* likely is how this went wrong the
/// first time. A pod submitted with `cpu: "10000"` comes back from the
/// apiserver as `cpu: "10k"`, because quantities are canonicalised to their
/// shortest form on the way in — so the string this ever sees is not
/// necessarily the string anyone wrote. `10k` fell through to a bare
/// `f64::parse`, failed, and became **zero**: the pod appeared to request no
/// CPU at all, `names()` returned nothing for it, the fit check skipped the
/// resource entirely, and a 10000-core pod was bound to a 4-core node. See
/// docs/E2E_FINDINGS.md finding 20.
///
/// `E` is ambiguous on purpose in the spec: `1E` is one exa, `1E3` is one
/// thousand. They are told apart by whether digits follow.
pub(crate) fn parse_quantity_f64(s: &str) -> Option<f64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let bytes = s.as_bytes();
    let mut i = 0;
    if bytes[i] == b'+' || bytes[i] == b'-' {
        i += 1;
    }
    while i < bytes.len() && (bytes[i].is_ascii_digit() || bytes[i] == b'.') {
        i += 1;
    }
    // Exponent form, but only when digits actually follow — otherwise this is
    // the exa suffix and belongs to the multiplier below.
    if i < bytes.len() && (bytes[i] == b'e' || bytes[i] == b'E') {
        let mut j = i + 1;
        if j < bytes.len() && (bytes[j] == b'+' || bytes[j] == b'-') {
            j += 1;
        }
        if j < bytes.len() && bytes[j].is_ascii_digit() {
            while j < bytes.len() && bytes[j].is_ascii_digit() {
                j += 1;
            }
            i = j;
        }
    }

    let (number, suffix) = s.split_at(i);
    let value: f64 = number.parse().ok()?;

    let multiplier = match suffix {
        "" => 1.0,
        "m" => 0.001,
        "k" => 1e3,
        "M" => 1e6,
        "G" => 1e9,
        "T" => 1e12,
        "P" => 1e15,
        "E" => 1e18,
        "Ki" => 1024.0,
        "Mi" => 1024.0 * 1024.0,
        "Gi" => 1024.0 * 1024.0 * 1024.0,
        "Ti" => 1024.0f64.powi(4),
        "Pi" => 1024.0f64.powi(5),
        "Ei" => 1024.0f64.powi(6),
        // Unknown suffix. Deliberately not "assume base units" — that is the
        // same fail-open guess that made `10k` read as zero.
        _ => return None,
    };
    Some(value * multiplier)
}

/// Parse a quantity to whole base units (bytes, or a plain count).
///
/// Rounds **up**: `1500m` of a countable resource is two whole units, and
/// rounding down would hand a pod less than it asked for.
pub fn parse_quantity(s: &str) -> i64 {
    match parse_quantity_f64(s) {
        Some(v) => v.ceil() as i64,
        None => {
            unparseable(s);
            0
        }
    }
}

/// Parse a quantity into **millis** — the representation CPU is stored in.
pub fn parse_quantity_milli(s: &str) -> i64 {
    match parse_quantity_f64(s) {
        // Milli is the canonical granularity for CPU, so every real value is
        // exact here and `round` is only guarding float representation.
        Some(v) => (v * 1000.0).round() as i64,
        None => {
            unparseable(s);
            0
        }
    }
}

/// A quantity the apiserver accepted but this cannot read.
///
/// Zero is returned because there is no safe answer that works for both
/// callers — a request that cannot be read should fail closed (never fit),
/// while an allocatable that cannot be read should fail closed the other way
/// (offer nothing), and both are "0" only for allocatable. What makes this
/// tolerable is that it should now be unreachable: the apiserver validates
/// quantities on admission and this understands every suffix it can emit. So
/// reaching here is a bug in this parser, and it must be loud rather than
/// silently making a pod look free.
fn unparseable(s: &str) {
    tracing::warn!(
        quantity = %s,
        "could not parse a resource quantity — treating it as zero, which will make \
         scheduling decisions wrong for this object. This should be unreachable; the \
         apiserver validates quantities. Please report it."
    );
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

/// One of the five in-tree volume sources `VolumeRestrictions` still checks
/// for cross-pod conflicts on the same node.
///
/// Every in-tree cloud volume plugin except these five was removed outright
/// (see docs/SCHEDULER.md, "Where the docs are wrong" — that removal is what
/// took `EBSLimits`/`GCEPDLimits`/`AzureDiskLimits` out of the default filter
/// list). These five volume *source* fields are still real, still validated
/// by the apiserver, and a cluster running two pods that reference the same
/// underlying disk on the same node is still a real double-attach outside
/// upstream's control — so the check stays, even though it will rarely fire
/// on an all-CSI cluster.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LegacyVolumeId {
    GcePersistentDisk { pd_name: String, read_only: bool },
    AwsElasticBlockStore { volume_id: String, read_only: bool },
    Iscsi { target_portal: String, iqn: String, lun: i32 },
    Rbd { monitors: Vec<String>, image: String, pool: String },
    Cinder { volume_id: String },
}

impl LegacyVolumeId {
    /// Whether two claims on these identities can coexist on one node.
    ///
    /// GCE PD and AWS EBS both allow two *read-only* attachments of the same
    /// disk to coexist — that is the one upstream nuance here, matching
    /// `gcePersistentDiskConflicts`/`awsElasticBlockStoreConflicts`. Every
    /// other kind conflicts unconditionally on identity: iSCSI targets, RBD
    /// images and Cinder volumes have no shared-read mode in these checks.
    pub fn conflicts_with(&self, other: &LegacyVolumeId) -> bool {
        match (self, other) {
            (
                LegacyVolumeId::GcePersistentDisk { pd_name: a, read_only: ro_a },
                LegacyVolumeId::GcePersistentDisk { pd_name: b, read_only: ro_b },
            ) => a == b && !(*ro_a && *ro_b),
            (
                LegacyVolumeId::AwsElasticBlockStore { volume_id: a, read_only: ro_a },
                LegacyVolumeId::AwsElasticBlockStore { volume_id: b, read_only: ro_b },
            ) => a == b && !(*ro_a && *ro_b),
            (
                LegacyVolumeId::Iscsi { target_portal: tp_a, iqn: iqn_a, lun: lun_a },
                LegacyVolumeId::Iscsi { target_portal: tp_b, iqn: iqn_b, lun: lun_b },
            ) => tp_a == tp_b && iqn_a == iqn_b && lun_a == lun_b,
            (
                LegacyVolumeId::Rbd { monitors: m_a, image: i_a, pool: p_a },
                LegacyVolumeId::Rbd { monitors: m_b, image: i_b, pool: p_b },
            ) => m_a == m_b && i_a == i_b && p_a == p_b,
            (
                LegacyVolumeId::Cinder { volume_id: a },
                LegacyVolumeId::Cinder { volume_id: b },
            ) => a == b,
            _ => false,
        }
    }
}

/// The scheduler's view of a pod.
#[derive(Clone, Debug, Default)]
pub struct PodInfo {
    /// Full object retained only when an HTTP extender is configured. The
    /// default path keeps this `None`, preserving the projection's memory
    /// advantage; extenders receive the exact API object upstream sends.
    pub api_object: Option<Box<Pod>>,
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
    /// The five in-tree volume sources `VolumeRestrictions` still checks for
    /// conflicts — see [`LegacyVolumeId`].
    pub legacy_volumes: Vec<LegacyVolumeId>,
    /// Driver names of this pod's ephemeral inline CSI volumes (`spec.volumes[].csi`,
    /// not a PVC). `NodeVolumeLimits` counts these against the same per-driver
    /// per-node ceiling as PVC-backed volumes — a driver has no way to tell
    /// the two apart once attached.
    pub csi_ephemeral_drivers: Vec<String>,
    /// `spec.resourceClaims`, for DRA — one entry per pod-claim, naming
    /// either an already-existing `ResourceClaim` or a template to generate
    /// one from.
    pub resource_claims: Vec<PodClaimRef>,
    /// `status.resourceClaimStatuses`, keyed by the pod-claim's own `name`
    /// (matching `resource_claims[].name`) — the *generated* `ResourceClaim`
    /// object name once the resource-claim controller has created one for a
    /// template-based entry. `None` means "no claim needed" (upstream writes
    /// this to say a template-based entry was intentionally skipped, not
    /// merely "not yet"); a `name` present in `resource_claims` but absent
    /// from this map means "not yet — still waiting on the controller".
    pub resource_claim_statuses: BTreeMap<String, Option<String>>,
    pub owner_references: Vec<OwnerRef>,
    /// When this pod entered the queue. Preserved across requeues — see the
    /// module header.
    pub queued_at: k8s_openapi::jiff::Timestamp,
    /// `status.startTime` — `None` for a pod that has never actually run yet
    /// (still Pending, or just assumed/bound with no kubelet report back
    /// yet). Used for preemption's equal-priority tiebreak
    /// (`preempt.rs`'s `more_important`) instead of `queued_at`: upstream's
    /// own `MoreImportantPod` compares real start time, not queue entry —
    /// a pod that has been *running* longer has more accumulated state to
    /// lose, which is a different thing from having been *waiting* longer.
    pub start_time: Option<k8s_openapi::jiff::Timestamp>,
    /// How many scheduling cycles this pod has already burned, for backoff.
    pub attempts: u32,
    /// Set by preemption; the node this pod has been promised.
    pub nominated_node_name: Option<String>,
    /// Existing PodScheduled condition, retained so reporting can preserve
    /// lastTransitionTime when the status remains False. Upstream changes
    /// that timestamp only when the condition status actually transitions.
    pub pod_scheduled_condition: Option<k8s_openapi::api::core::v1::PodCondition>,
}

/// A weighted preferred node-affinity term, flattened out of the API shape so
/// scoring does not re-walk `Option` chains per node.
#[derive(Clone, Debug)]
pub struct PreferredTerm {
    pub weight: i32,
    pub selector: k8s_openapi::api::core::v1::NodeSelectorTerm,
}

/// One `spec.resourceClaims[]` entry — a pod-local claim name, and how to
/// find the real object: either directly, or via a template that still
/// needs `resource_claim_statuses` to resolve.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PodClaimRef {
    pub name: String,
    pub resource_claim_name: Option<String>,
    pub resource_claim_template_name: Option<String>,
}

impl PodClaimRef {
    /// The real `ResourceClaim` object name to fetch — direct is a pure
    /// pass-through; template-based needs the generated name the
    /// resource-claim controller recorded in `status.resourceClaimStatuses`
    /// (keyed by this pod-claim's own `name`, not the template's).
    /// `None` if that hasn't happened yet. Mirrors
    /// `crates/nodelet/src/runtime/cri/claims.rs`'s
    /// `resource_claim_object_name()` exactly — not shared code (the two
    /// components share none, per CLAUDE.md), the same small pure function
    /// independently re-derived twice because both genuinely need it.
    pub fn object_name(&self, statuses: &BTreeMap<String, Option<String>>) -> Option<String> {
        if let Some(name) = &self.resource_claim_name {
            return Some(name.clone());
        }
        statuses.get(&self.name).cloned().flatten()
    }
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
    pub fn from_pod(pod: &Pod, queued_at: k8s_openapi::jiff::Timestamp) -> Self {
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
            images.push(normalized_image_name(&c.image.clone().unwrap_or_default()));
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
            api_object: None,
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
            // The same container set `images` was built from — init
            // containers contribute image entries too, and ImageLocality's
            // cap scales with this count, so counting only spec.containers
            // let init-container images carry excess weight.
            container_count: images.len(),
            images,
            pvc_names: spec
                .volumes
                .iter()
                .flatten()
                .filter_map(|v| {
                    v.persistent_volume_claim.as_ref().map(|p| p.claim_name.clone())
                })
                .collect(),
            legacy_volumes: spec.volumes.iter().flatten().filter_map(legacy_volume_id).collect(),
            csi_ephemeral_drivers: spec
                .volumes
                .iter()
                .flatten()
                .filter_map(|v| v.csi.as_ref().map(|c| c.driver.clone()))
                .collect(),
            resource_claims: spec
                .resource_claims
                .iter()
                .flatten()
                .map(|c| PodClaimRef {
                    name: c.name.clone(),
                    resource_claim_name: c.resource_claim_name.clone(),
                    resource_claim_template_name: c.resource_claim_template_name.clone(),
                })
                .collect(),
            resource_claim_statuses: pod
                .status
                .as_ref()
                .and_then(|s| s.resource_claim_statuses.as_ref())
                .into_iter()
                .flat_map(|v| v.iter())
                .map(|s| (s.name.clone(), s.resource_claim_name.clone()))
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
            start_time: pod.status.as_ref().and_then(|s| s.start_time.clone()).map(|t| t.0),
            attempts: 0,
            nominated_node_name: pod
                .status
                .as_ref()
                .and_then(|s| s.nominated_node_name.clone()),
            pod_scheduled_condition: pod
                .status
                .as_ref()
                .and_then(|s| s.conditions.as_ref())
                .into_iter()
                .flatten()
                .find(|condition| condition.type_ == "PodScheduled")
                .cloned(),
        }
    }
}

/// Match kube-scheduler's CRI image-name normalization: a reference without
/// a tag after its final slash implicitly means `:latest`. A registry port
/// (`registry:5000/image`) is not itself an image tag.
fn normalized_image_name(name: &str) -> String {
    let needs_latest = match name.rfind(':') {
        None => true,
        Some(colon) => name.rfind('/').is_some_and(|slash| colon <= slash),
    };
    if needs_latest { format!("{name}:latest") } else { name.to_string() }
}

/// Pull a [`LegacyVolumeId`] out of a pod's volume source, if it is one of
/// the five kinds `VolumeRestrictions` still checks. Every other volume type
/// — including every CSI volume, which is what this project's own reference
/// deploys use — projects to `None`.
fn legacy_volume_id(v: &k8s_openapi::api::core::v1::Volume) -> Option<LegacyVolumeId> {
    if let Some(gce) = &v.gce_persistent_disk {
        return Some(LegacyVolumeId::GcePersistentDisk {
            pd_name: gce.pd_name.clone(),
            read_only: gce.read_only.unwrap_or(false),
        });
    }
    if let Some(ebs) = &v.aws_elastic_block_store {
        return Some(LegacyVolumeId::AwsElasticBlockStore {
            volume_id: ebs.volume_id.clone(),
            read_only: ebs.read_only.unwrap_or(false),
        });
    }
    if let Some(iscsi) = &v.iscsi {
        return Some(LegacyVolumeId::Iscsi {
            target_portal: iscsi.target_portal.clone(),
            iqn: iscsi.iqn.clone(),
            lun: iscsi.lun,
        });
    }
    if let Some(rbd) = &v.rbd {
        return Some(LegacyVolumeId::Rbd {
            monitors: rbd.monitors.clone(),
            image: rbd.image.clone(),
            pool: rbd.pool.clone().unwrap_or_else(|| "rbd".to_string()),
        });
    }
    if let Some(cinder) = &v.cinder {
        return Some(LegacyVolumeId::Cinder { volume_id: cinder.volume_id.clone() });
    }
    None
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

    // InPlacePodVerticalScaling is GA and enabled in 1.33. During a resize,
    // scheduling must reserve the maximum of desired, actually applied, and
    // allocated resources so neither an unfinished scale-up nor a deferred
    // scale-down makes the node look artificially empty. If kubelet declared
    // the resize infeasible, upstream stops considering the unattainable new
    // spec and uses max(actual, allocated) instead.
    let resize_infeasible = pod
        .status
        .as_ref()
        .and_then(|status| status.conditions.as_ref())
        .into_iter()
        .flatten()
        .any(|condition| {
            condition.type_ == "PodResizePending" && condition.reason.as_deref() == Some("Infeasible")
        });
    let statuses: BTreeMap<&str, &k8s_openapi::api::core::v1::ContainerStatus> = pod
        .status
        .as_ref()
        .into_iter()
        .flat_map(|status| {
            status
                .container_statuses
                .iter()
                .flatten()
                .chain(status.init_container_statuses.iter().flatten())
        })
        .map(|status| (status.name.as_str(), status))
        .collect();

    let effective = |container: &k8s_openapi::api::core::v1::Container,
                     use_status: bool| {
        let desired = container
            .resources
            .as_ref()
            .and_then(|resources| resources.requests.as_ref())
            .map(Resources::from_quantities)
            .unwrap_or_default();
        if !use_status {
            return desired;
        }
        let Some(status) = statuses.get(container.name.as_str()) else {
            return desired;
        };
        if status.resources.is_none() {
            return desired;
        }
        let actual = status
            .resources
            .as_ref()
            .and_then(|resources| resources.requests.as_ref())
            .map(Resources::from_quantities)
            .unwrap_or_default();
        let allocated = status
            .allocated_resources
            .as_ref()
            .map(Resources::from_quantities)
            .unwrap_or_default();
        let mut result = if resize_infeasible { actual.clone() } else { desired };
        result.max_with(&actual);
        result.max_with(&allocated);
        result
    };

    let mut sum = Resources::default();
    for c in &spec.containers {
        sum.add(&effective(c, true));
    }

    let mut restartable_init_sum = Resources::default();
    let mut init_max = Resources::default();
    for c in spec.init_containers.iter().flatten() {
        let restartable = c.restart_policy.as_deref() == Some("Always");
        let r = effective(c, restartable);
        let mut phase = r.clone();
        if restartable {
            sum.add(&r);
            restartable_init_sum.add(&r);
            phase = restartable_init_sum.clone();
        } else {
            // Every earlier restartable init container is still running while
            // this ordinary init container executes.
            phase.add(&restartable_init_sum);
        }
        init_max.max_with(&phase);
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
