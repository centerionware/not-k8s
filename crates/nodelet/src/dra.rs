//! Dynamic Resource Allocation (DRA, `spec.resourceClaims`): the
//! kubelet-side client for a DRA driver's own `NodePrepareResources`/
//! `NodeUnprepareResources` gRPC plugin API (round 63; see
//! `proto/draplugin.proto` for the protocol itself and its accuracy
//! caveat).
//!
//! **Reuses the existing plugin-registration infrastructure directly** —
//! same as device plugins (round 14) and CSI drivers (round 12/13), a DRA
//! driver's own `node-driver-registrar`-equivalent sidecar registers
//! through the exact same `GetInfo`/`NotifyRegistrationStatus` handshake
//! (see `plugin_registry.rs`), just with `PluginInfo.type ==
//! "DRAPlugin"`.
//!
//! Unlike device plugins, a DRA driver has no `ListAndWatch` inventory
//! stream to kubelet — kubelet only ever *fetches* allocation results from
//! the `ResourceClaim` object itself (already decided by the scheduler +
//! the driver's own control-plane component) and asks the driver to
//! *prepare*/*unprepare* specific already-allocated devices. So this
//! module is just a name -> endpoint registry (`register()`/
//! `deregister()`, identical shape to `runtime::csi::CsiDrivers`'
//! `endpoints` map) plus the two RPC calls themselves.
//!
//! What's genuinely kubelet's job here, and what isn't: reading
//! `ResourceClaim.status.allocation` and translating driver-returned CDI
//! device IDs into the container's CRI `cdi_devices` is kubelet's job
//! (implemented in `runtime/cri.rs`). *Allocating* a claim (picking which
//! devices satisfy it) is the scheduler's/a DRA driver's control-plane
//! component's job — nodelet only ever reads an allocation already made,
//! never computes one.

use anyhow::{Context, Result};
use std::collections::HashMap;
use std::sync::Mutex;
use tonic::transport::{Channel, Endpoint, Uri};

// Package name matches proto/draplugin.proto's real upstream package
// (k8s.io.kubelet.pkg.apis.dra.v1 — see that file's own doc comment for
// round 121's "this used to be wrong" story) exactly, since tonic's
// generated code is looked up by that literal string.
pub mod v1 {
    tonic::include_proto!("k8s.io.kubelet.pkg.apis.dra.v1");
}

use v1::dra_plugin_client::DraPluginClient;
use v1::{Claim, Device, NodePrepareResourcesRequest, NodeUnprepareResourcesRequest};

/// Dial a DRA driver's own Unix socket — same connector shape as every
/// other `connect_uds` in this codebase (`runtime/cri.rs`, `runtime/csi.rs`,
/// `device_plugins.rs`, `plugin_registry.rs`).
async fn connect_uds(endpoint: &str) -> Result<Channel> {
    let path = endpoint.strip_prefix("unix://").unwrap_or(endpoint).to_string();
    Endpoint::try_from("http://localhost")
        .context("invalid endpoint")?
        .connect_with_connector(tower::service_fn(move |_: Uri| {
            let path = path.clone();
            async move {
                let stream = tokio::net::UnixStream::connect(path).await?;
                Ok::<_, std::io::Error>(hyper_util::rt::TokioIo::new(stream))
            }
        }))
        .await
        .context("connecting to DRA plugin unix socket")
}

/// Identifies one `ResourceClaim` object for a `NodePrepareResources`/
/// `NodeUnprepareResources` call.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClaimRef {
    pub namespace: String,
    pub uid: String,
    pub name: String,
}

impl From<&ClaimRef> for Claim {
    fn from(c: &ClaimRef) -> Self {
        Claim { namespace: c.namespace.clone(), uid: c.uid.clone(), name: c.name.clone() }
    }
}

/// One prepared device: which request(s) in the claim it satisfies, and
/// its CDI device IDs — pure data, mirrors `v1::Device` without depending
/// on callers reaching into the generated proto type directly.
/// `pool_name`/`device_name` (identifying which specific device this is,
/// independent of CDI) and `share_id` aren't currently consumed by
/// anything nodelet does with a prepared device (CDI IDs are all
/// `container_create.rs` ever injects), so they're not carried into this
/// type — round 121 found and fixed the real bug here (wrong proto
/// package/field layout entirely, see `proto/draplugin.proto`), not a
/// reason to widen this struct without an actual consumer yet.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct PreparedDevice {
    pub request_names: Vec<String>,
    pub cdi_device_ids: Vec<String>,
}

fn from_proto_devices(devices: Vec<Device>) -> Vec<PreparedDevice> {
    devices.into_iter().map(|d| PreparedDevice { request_names: d.request_names, cdi_device_ids: d.cdi_device_ids }).collect()
}

/// Map a `NodePrepareResourcesResponse`'s per-claim-UID results back onto
/// the `claims` that were actually requested — pure so this is
/// unit-testable without a live driver socket. Every claim in `claims`
/// gets exactly one entry in the output: `Ok` with its prepared devices,
/// or `Err` with either the driver's own reported error message or a
/// synthetic one if the driver's response silently omitted that claim
/// (a malformed/buggy driver response, not something to treat as success).
fn map_prepare_results(claims: &[ClaimRef], resp_claims: &HashMap<String, v1::NodePrepareResourceResponse>) -> HashMap<String, Result<Vec<PreparedDevice>, String>> {
    claims
        .iter()
        .map(|c| {
            let result = match resp_claims.get(&c.uid) {
                Some(r) if r.error.is_empty() => Ok(from_proto_devices(r.devices.clone())),
                Some(r) => Err(r.error.clone()),
                None => Err(format!("NodePrepareResources response missing claim uid '{}'", c.uid)),
            };
            (c.uid.clone(), result)
        })
        .collect()
}

/// `map_prepare_results()`'s counterpart for `NodeUnprepareResources` — a
/// claim the driver's response silently omits is treated as `Ok` (already
/// gone / nothing to unprepare), not an error: unlike a missing prepare
/// result (which hides real device state the caller needs), an absent
/// unprepare result has no useful signal either way, and treating it as a
/// failure would just make routine teardown noisier for no benefit.
fn map_unprepare_results(claims: &[ClaimRef], resp_claims: &HashMap<String, v1::NodeUnprepareResourceResponse>) -> HashMap<String, Result<(), String>> {
    claims
        .iter()
        .map(|c| {
            let result = match resp_claims.get(&c.uid) {
                Some(r) if r.error.is_empty() => Ok(()),
                Some(r) => Err(r.error.clone()),
                None => Ok(()),
            };
            (c.uid.clone(), result)
        })
        .collect()
}

pub struct DraDrivers {
    /// driver name -> plugin endpoint, seeded entirely via dynamic
    /// registration (no static-config equivalent — a DRA driver's
    /// endpoint is meaningless without the driver process itself running
    /// to answer prepare/unprepare calls, so there's nothing useful a
    /// static config could pre-declare).
    endpoints: Mutex<HashMap<String, String>>,
}

impl Default for DraDrivers {
    fn default() -> Self {
        Self::new()
    }
}

impl DraDrivers {
    pub fn new() -> Self {
        Self { endpoints: Mutex::new(HashMap::new()) }
    }

    pub fn register(&self, driver_name: String, endpoint: String) {
        self.endpoints.lock().unwrap().insert(driver_name, endpoint);
    }

    pub fn deregister(&self, driver_name: &str) {
        self.endpoints.lock().unwrap().remove(driver_name);
    }

    pub fn driver_configured(&self, driver_name: &str) -> bool {
        self.endpoints.lock().unwrap().contains_key(driver_name)
    }

    /// Ask `driver` to prepare every claim in `claims` — one
    /// `NodePrepareResources` RPC covering all of them (round 64; the real
    /// protocol's `claims` field is a list precisely so a pod referencing
    /// several claims backed by the same driver doesn't need a separate
    /// round-trip per claim — round 63's first cut called this once per
    /// claim instead). Returns a per-claim-UID result: an individual
    /// claim's own failure (reported via the response's `error` field)
    /// doesn't fail the whole batch, since other claims in the same
    /// response may have succeeded. The outer `Result` only fails for a
    /// batch-wide problem (driver not registered, unreachable, or the RPC
    /// itself erroring).
    pub async fn prepare(&self, driver: &str, claims: &[ClaimRef]) -> Result<HashMap<String, Result<Vec<PreparedDevice>, String>>> {
        if claims.is_empty() {
            return Ok(HashMap::new());
        }
        let endpoint = self.endpoints.lock().unwrap().get(driver).cloned();
        let Some(endpoint) = endpoint else {
            anyhow::bail!("no registered DRA driver named '{driver}'");
        };
        let channel = connect_uds(&endpoint).await?;
        let mut client = DraPluginClient::new(channel);
        let resp = client
            .node_prepare_resources(NodePrepareResourcesRequest { claims: claims.iter().map(Claim::from).collect() })
            .await
            .context("NodePrepareResources")?
            .into_inner();
        Ok(map_prepare_results(claims, &resp.claims))
    }

    /// Ask `driver` to release whatever `prepare()` set up for every claim
    /// in `claims` — one `NodeUnprepareResources` RPC covering all of
    /// them, same batching reasoning as `prepare()`. Called once every
    /// container referencing them has been removed. Best-effort from the
    /// caller's perspective (see `runtime/cri.rs`'s call site): logged,
    /// not fatal to pod teardown, same posture as `unmount_csi_volumes()`.
    pub async fn unprepare(&self, driver: &str, claims: &[ClaimRef]) -> Result<HashMap<String, Result<(), String>>> {
        if claims.is_empty() {
            return Ok(HashMap::new());
        }
        let endpoint = self.endpoints.lock().unwrap().get(driver).cloned();
        let Some(endpoint) = endpoint else {
            anyhow::bail!("no registered DRA driver named '{driver}'");
        };
        let channel = connect_uds(&endpoint).await?;
        let mut client = DraPluginClient::new(channel);
        let resp = client
            .node_unprepare_resources(NodeUnprepareResourcesRequest { claims: claims.iter().map(Claim::from).collect() })
            .await
            .context("NodeUnprepareResources")?
            .into_inner();
        Ok(map_unprepare_results(claims, &resp.claims))
    }
}

#[cfg(test)]
#[path = "dra_tests/claim_ref.rs"]
mod tests_claim_ref;
#[cfg(test)]
#[path = "dra_tests/batch_results.rs"]
mod tests_batch_results;
