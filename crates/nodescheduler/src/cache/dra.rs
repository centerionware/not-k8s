//! Hand-written subset of `resource.k8s.io`'s DRA types — `ResourceClaim`,
//! `DeviceClass`, `ResourceSlice`. `resource.k8s.io/v1` is typed as of the
//! k8s-openapi `v1_34` bump (see `CLAUDE.md`), but this raw-request copy
//! hasn't been retired yet — that's separate follow-up work, not a rider on
//! the version bump (`docs/APISERVER_PLAN.md` Phase 0). Same pattern
//! `crates/nodelet/src/runtime/cri/claims.rs`'s `RawResourceClaim` already
//! uses on the node side: fetched/watched via a raw request into these
//! structs rather than a typed `kube::Api`.
//!
//! Unlike `nodelet`'s copy (which only ever *reads* `status.allocation` and
//! `status.reservedFor` — someone else already wrote them), this scheduler
//! is the one that **decides** an allocation and writes both fields, so this
//! copy also carries `spec` in full and the write-side status shapes.
//!
//! Field names are `camelCase` in JSON. The v1beta1 API's Rust field names
//! (checked directly against k8s-openapi's vendored v1beta1 schema, which
//! *is* present at this pin) are used as the source of truth for what the
//! stable v1 API actually serialises — the JSON shape is identical between
//! the two; only the Go/Rust type names changed on the way to GA.
//!
//! # Why these implement `k8s_openapi::Resource`/`Metadata` by hand
//!
//! `kube::Api<K>`/`kube::runtime::watcher` need `K: kube::Resource`, which
//! `kube-core` gives for free to any `K: k8s_openapi::Metadata<Ty = ObjectMeta>
//! + k8s_openapi::Resource` — the same blanket impl every generated
//! k8s-openapi type rides on. These three implement that pair by hand
//! instead of being generated, so `watch.rs` can watch them exactly the way
//! it watches every other resource, with no special case.

use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use k8s_openapi::{ClusterResourceScope, NamespaceResourceScope};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

// ─────────────────────────────────────────────────────────────────────────
// ResourceClaim
// ─────────────────────────────────────────────────────────────────────────

#[derive(Deserialize, Default, Clone, Debug)]
pub struct RawResourceClaim {
    #[serde(default)]
    pub metadata: ObjectMeta,
    #[serde(default)]
    pub spec: RawResourceClaimSpec,
    pub status: Option<RawResourceClaimStatus>,
}

impl k8s_openapi::Resource for RawResourceClaim {
    const API_VERSION: &'static str = "resource.k8s.io/v1";
    const GROUP: &'static str = "resource.k8s.io";
    const KIND: &'static str = "ResourceClaim";
    const VERSION: &'static str = "v1";
    const URL_PATH_SEGMENT: &'static str = "resourceclaims";
    type Scope = NamespaceResourceScope;
}

impl k8s_openapi::Metadata for RawResourceClaim {
    type Ty = ObjectMeta;
    fn metadata(&self) -> &ObjectMeta {
        &self.metadata
    }
    fn metadata_mut(&mut self) -> &mut ObjectMeta {
        &mut self.metadata
    }
}

#[derive(Deserialize, Default, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct RawResourceClaimSpec {
    pub devices: Option<RawDeviceClaim>,
}

#[derive(Deserialize, Default, Clone, Debug)]
pub struct RawDeviceClaim {
    pub requests: Option<Vec<RawDeviceRequest>>,
    /// Cross-request "must share an attribute" rules — evaluated in
    /// `dynamic_resources.rs`'s `allocate_on_node` as devices are picked, the
    /// same "first device in the group sets the value, later ones must
    /// match" rule upstream's `matchAttributeConstraint` uses. The allocator
    /// backtracks across requests and claims when an early match prevents a
    /// complete solution.
    pub constraints: Option<Vec<RawDeviceConstraint>>,
    /// Driver configuration supplied by the claim. It does not participate
    /// in selection, but must be copied into the allocation result for the
    /// node-side DRA plugin.
    pub config: Option<Vec<RawDeviceClaimConfiguration>>,
}

#[derive(Deserialize, Serialize, Clone, Debug, PartialEq)]
pub struct RawOpaqueDeviceConfiguration {
    pub driver: String,
    pub parameters: serde_json::Value,
}

#[derive(Deserialize, Clone, Debug)]
pub struct RawDeviceClaimConfiguration {
    #[serde(default)]
    pub requests: Vec<String>,
    pub opaque: Option<RawOpaqueDeviceConfiguration>,
}

#[derive(Deserialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct RawDeviceConstraint {
    /// The only constraint kind the real API defines as of 1.33 — a fully
    /// qualified (or class-domain-relative) attribute name every selected
    /// device covered by this constraint must share the same value of.
    pub match_attribute: Option<String>,
    /// Which requests (by name, or `"request/subrequest"` for a
    /// `firstAvailable` pick) this constraint applies to. Empty/absent means
    /// every request in the claim.
    #[serde(default)]
    pub requests: Vec<String>,
}

/// The stable `v1` API's real shape, confirmed against a live cluster
/// (`kubectl explain resourceclaim.spec.devices.requests`) rather than
/// assumed from `v1beta1`: the per-request fields this crate cares about are
/// not flat on `DeviceRequest` — they live under `exactly`, alongside
/// `firstAvailable` as the other arm of a "one of" the real API added on the
/// way to GA. Getting this nesting wrong doesn't fail to compile or even to
/// deserialize (serde silently defaults an absent `Option` field to `None`)
/// — it silently treats every real claim as using a DRA feature this crate
/// doesn't implement, which is why this is called out here rather than left
/// to look like an obvious flat struct.
#[derive(Deserialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct RawDeviceRequest {
    pub name: String,
    pub exactly: Option<RawExactDeviceRequest>,
    /// Beta in 1.33 (`DRAPrioritizedList`): a prioritised list of alternative
    /// requests, each potentially naming a different device class, tried in
    /// order — the first one `allocate_on_node` can fully satisfy wins, same
    /// as upstream's own `allocateOne` subrequest loop.
    #[serde(default)]
    pub first_available: Option<Vec<RawDeviceSubRequest>>,
}

/// One alternative inside a `firstAvailable` list. Deliberately its own type
/// rather than reusing `RawExactDeviceRequest`: a subrequest has no
/// `adminAccess` field at all in the real API (upstream's own
/// `deviceSubRequestAccessor.adminAccess()` is hardcoded `false` — see
/// `allocator.go`), so giving it one here would silently accept a field the
/// apiserver itself never lets a subrequest set.
#[derive(Deserialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct RawDeviceSubRequest {
    pub name: String,
    pub device_class_name: Option<String>,
    pub selectors: Option<Vec<RawDeviceSelector>>,
    pub allocation_mode: Option<String>,
    pub count: Option<i64>,
}

#[derive(Deserialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct RawExactDeviceRequest {
    pub device_class_name: Option<String>,
    pub selectors: Option<Vec<RawDeviceSelector>>,
    pub allocation_mode: Option<String>,
    pub count: Option<i64>,
    #[serde(default)]
    pub admin_access: Option<bool>,
}

#[derive(Deserialize, Clone, Debug)]
pub struct RawDeviceSelector {
    pub cel: Option<RawCelSelector>,
}

#[derive(Deserialize, Clone, Debug)]
pub struct RawCelSelector {
    pub expression: String,
}

#[derive(Deserialize, Default, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct RawResourceClaimStatus {
    pub allocation: Option<RawAllocationResult>,
    pub reserved_for: Option<Vec<RawConsumerReference>>,
}

#[derive(Deserialize, Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct RawAllocationResult {
    pub devices: Option<RawDeviceAllocationResult>,
    pub node_selector: Option<k8s_openapi::api::core::v1::NodeSelector>,
}

#[derive(Deserialize, Serialize, Clone, Debug, Default)]
pub struct RawDeviceAllocationResult {
    pub results: Option<Vec<RawDeviceRequestAllocationResult>>,
    pub config: Option<Vec<RawDeviceAllocationConfiguration>>,
}

#[derive(Deserialize, Serialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct RawDeviceAllocationConfiguration {
    pub source: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub requests: Vec<String>,
    pub opaque: Option<RawOpaqueDeviceConfiguration>,
}

#[derive(Deserialize, Serialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RawDeviceRequestAllocationResult {
    pub request: String,
    pub driver: String,
    pub pool: String,
    pub device: String,
    #[serde(default, skip_serializing_if = "is_false")]
    pub admin_access: bool,
}

fn is_false(b: &bool) -> bool {
    !*b
}

#[derive(Deserialize, Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct RawConsumerReference {
    #[serde(default, rename = "apiGroup")]
    pub api_group: Option<String>,
    pub resource: String,
    pub name: String,
    pub uid: String,
}

// ─────────────────────────────────────────────────────────────────────────
// DeviceClass
// ─────────────────────────────────────────────────────────────────────────

#[derive(Deserialize, Default, Clone, Debug)]
pub struct RawDeviceClass {
    #[serde(default)]
    pub metadata: ObjectMeta,
    #[serde(default)]
    pub spec: RawDeviceClassSpec,
}

impl k8s_openapi::Resource for RawDeviceClass {
    const API_VERSION: &'static str = "resource.k8s.io/v1";
    const GROUP: &'static str = "resource.k8s.io";
    const KIND: &'static str = "DeviceClass";
    const VERSION: &'static str = "v1";
    const URL_PATH_SEGMENT: &'static str = "deviceclasses";
    type Scope = ClusterResourceScope;
}

impl k8s_openapi::Metadata for RawDeviceClass {
    type Ty = ObjectMeta;
    fn metadata(&self) -> &ObjectMeta {
        &self.metadata
    }
    fn metadata_mut(&mut self) -> &mut ObjectMeta {
        &mut self.metadata
    }
}

#[derive(Deserialize, Default, Clone, Debug)]
pub struct RawDeviceClassSpec {
    pub selectors: Option<Vec<RawDeviceSelector>>,
    pub config: Option<Vec<RawDeviceClassConfiguration>>,
}

#[derive(Deserialize, Clone, Debug)]
pub struct RawDeviceClassConfiguration {
    pub opaque: Option<RawOpaqueDeviceConfiguration>,
}

// ─────────────────────────────────────────────────────────────────────────
// ResourceSlice
// ─────────────────────────────────────────────────────────────────────────

#[derive(Deserialize, Default, Clone, Debug)]
pub struct RawResourceSlice {
    #[serde(default)]
    pub metadata: ObjectMeta,
    #[serde(default)]
    pub spec: RawResourceSliceSpec,
}

impl k8s_openapi::Resource for RawResourceSlice {
    const API_VERSION: &'static str = "resource.k8s.io/v1";
    const GROUP: &'static str = "resource.k8s.io";
    const KIND: &'static str = "ResourceSlice";
    const VERSION: &'static str = "v1";
    const URL_PATH_SEGMENT: &'static str = "resourceslices";
    type Scope = ClusterResourceScope;
}

impl k8s_openapi::Metadata for RawResourceSlice {
    type Ty = ObjectMeta;
    fn metadata(&self) -> &ObjectMeta {
        &self.metadata
    }
    fn metadata_mut(&mut self) -> &mut ObjectMeta {
        &mut self.metadata
    }
}

#[derive(Deserialize, Default, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct RawResourceSliceSpec {
    pub driver: String,
    pub pool: RawResourcePool,
    pub node_name: Option<String>,
    pub all_nodes: Option<bool>,
    /// A real `v1.NodeSelector` (matchExpressions/matchFields terms), *not*
    /// a `metav1.LabelSelector` — confirmed against the real Go type
    /// (`ResourceSliceSpec.NodeSelector *v1.NodeSelector`). Getting this
    /// wrong doesn't fail to compile (both deserialize from similar-looking
    /// JSON) — it silently treats every real `matchExpressions` term as a
    /// `LabelSelectorRequirement` instead of a `NodeSelectorRequirement`,
    /// which happen to share JSON shape for `key`/`operator`/`values` but
    /// mean different things once evaluated. Exactly one of `node_name`,
    /// `node_selector`, `all_nodes`, `per_device_node_selection` is set —
    /// see `snapshot.rs`'s `resource_slices_for_node`.
    pub node_selector: Option<k8s_openapi::api::core::v1::NodeSelector>,
    /// When set, node applicability is decided per device (each device's own
    /// `node_name`/`node_selector`/`all_nodes`) instead of once for the
    /// whole slice — see `RawBasicDevice`'s fields and
    /// `dynamic_resources.rs`'s `candidates_for_node`.
    pub per_device_node_selection: Option<bool>,
    pub devices: Option<Vec<RawDevice>>,
}

#[derive(Deserialize, Default, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct RawResourcePool {
    pub name: String,
    pub generation: Option<i64>,
    pub resource_slice_count: Option<i64>,
}

/// The stable `v1` API's real shape, confirmed live the same way
/// `RawDeviceRequest`'s doc comment describes: `attributes`/`capacity` are
/// flat on `Device` (`kubectl explain resourceslice.spec.devices`), not
/// nested under a `basic` object the way `v1beta1` had it. `#[serde(flatten)]`
/// reads them off the same JSON object `name` is on while keeping
/// `RawBasicDevice` as a reusable payload type for the rest of this file
/// (CEL device-matching only ever needs attributes+capacity, not `name`).
#[derive(Deserialize, Clone, Debug)]
pub struct RawDevice {
    pub name: String,
    #[serde(flatten)]
    pub basic: RawBasicDevice,
}

#[derive(Deserialize, Default, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct RawBasicDevice {
    pub attributes: Option<BTreeMap<String, RawDeviceAttribute>>,
    pub capacity: Option<BTreeMap<String, RawDeviceCapacity>>,
    /// Only meaningful (and only ever set) when the owning slice's
    /// `perDeviceNodeSelection` is true — see `RawResourceSliceSpec`'s field
    /// and `candidates_for_node` in `dynamic_resources.rs`. At most one of
    /// these three is set, same "exactly one of" shape the slice level has.
    pub node_name: Option<String>,
    pub node_selector: Option<k8s_openapi::api::core::v1::NodeSelector>,
    pub all_nodes: Option<bool>,
}

#[derive(Deserialize, Clone, Debug)]
pub struct RawDeviceAttribute {
    pub bool: Option<bool>,
    pub int: Option<i64>,
    pub string: Option<String>,
    pub version: Option<String>,
}

#[derive(Deserialize, Clone, Debug)]
pub struct RawDeviceCapacity {
    pub value: k8s_openapi::apimachinery::pkg::api::resource::Quantity,
}

impl RawResourceClaim {
    pub fn key(&self) -> String {
        format!(
            "{}/{}",
            self.metadata.namespace.as_deref().unwrap_or_default(),
            self.metadata.name.as_deref().unwrap_or_default()
        )
    }

    pub fn bound(&self) -> bool {
        self.status.as_ref().is_some_and(|s| s.allocation.is_some())
    }
}
