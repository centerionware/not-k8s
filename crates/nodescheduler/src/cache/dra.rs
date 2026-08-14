//! Hand-written subset of `resource.k8s.io`'s DRA types — `ResourceClaim`,
//! `DeviceClass`, `ResourceSlice`. Same reason and same pattern as
//! `cache/storage.rs`'s comment on `resource.k8s.io/v1` not existing in the
//! pinned k8s-openapi `v1_33` schema feature, and the same one
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

use k8s_openapi::apimachinery::pkg::apis::meta::v1::{LabelSelector, ObjectMeta};
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
    /// Cross-request "must share an attribute" rules. Not evaluated — see
    /// this module's header and `dynamic_resources.rs`'s scope note. A claim
    /// using this is allocated as if it were absent, which is a real,
    /// documented narrowing rather than a silent one.
    pub constraints: Option<serde_json::Value>,
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
    /// Alpha/beta in v1.33+ (a prioritised list of alternative requests, each
    /// potentially naming a different device class). Not evaluated — a
    /// request naming this is unschedulable rather than silently allocated
    /// against one arbitrary alternative.
    pub first_available: Option<serde_json::Value>,
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
    /// Alpha/beta topology-selection modes this crate does not resolve — a
    /// slice using either is treated as having no devices available on any
    /// node, rather than guessed at.
    pub node_selector: Option<LabelSelector>,
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
pub struct RawBasicDevice {
    pub attributes: Option<BTreeMap<String, RawDeviceAttribute>>,
    pub capacity: Option<BTreeMap<String, RawDeviceCapacity>>,
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
