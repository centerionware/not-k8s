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

pub mod v1beta1 {
    tonic::include_proto!("dra.v1beta1");
}

use v1beta1::dra_plugin_client::DraPluginClient;
use v1beta1::{Claim, Device, NodePrepareResourcesRequest, NodeUnprepareResourcesRequest};

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
/// its CDI device IDs — pure data, mirrors `v1beta1::Device` without
/// depending on callers reaching into the generated proto type directly.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct PreparedDevice {
    pub request_names: Vec<String>,
    pub cdi_device_ids: Vec<String>,
}

fn from_proto_devices(devices: Vec<Device>) -> Vec<PreparedDevice> {
    devices.into_iter().map(|d| PreparedDevice { request_names: d.request_names, cdi_device_ids: d.cdi_device_ids }).collect()
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

    /// Ask `driver` to prepare `claim` — the devices the scheduler/driver
    /// already allocated to it become actually usable (e.g. bound into a
    /// CDI-visible state) on this node. Returns the prepared devices, or
    /// an error if the driver isn't registered, unreachable, or reports
    /// its own per-claim failure via the response's `error` field.
    pub async fn prepare(&self, driver: &str, claim: &ClaimRef) -> Result<Vec<PreparedDevice>> {
        let endpoint = self.endpoints.lock().unwrap().get(driver).cloned();
        let Some(endpoint) = endpoint else {
            anyhow::bail!("no registered DRA driver named '{driver}'");
        };
        let channel = connect_uds(&endpoint).await?;
        let mut client = DraPluginClient::new(channel);
        let resp = client
            .node_prepare_resources(NodePrepareResourcesRequest { claims: vec![Claim::from(claim)] })
            .await
            .context("NodePrepareResources")?
            .into_inner();
        let result = resp
            .claims
            .get(&claim.uid)
            .with_context(|| format!("NodePrepareResources response missing claim uid '{}'", claim.uid))?;
        if !result.error.is_empty() {
            anyhow::bail!("driver '{driver}' failed to prepare claim '{}': {}", claim.name, result.error);
        }
        Ok(from_proto_devices(result.devices.clone()))
    }

    /// Ask `driver` to release whatever `prepare()` set up for `claim` —
    /// called once every container referencing it has been removed.
    /// Best-effort from the caller's perspective (see `runtime/cri.rs`'s
    /// call site): logged, not fatal to pod teardown, same posture as
    /// `unmount_csi_volumes()`.
    pub async fn unprepare(&self, driver: &str, claim: &ClaimRef) -> Result<()> {
        let endpoint = self.endpoints.lock().unwrap().get(driver).cloned();
        let Some(endpoint) = endpoint else {
            anyhow::bail!("no registered DRA driver named '{driver}'");
        };
        let channel = connect_uds(&endpoint).await?;
        let mut client = DraPluginClient::new(channel);
        let resp = client
            .node_unprepare_resources(NodeUnprepareResourcesRequest { claims: vec![Claim::from(claim)] })
            .await
            .context("NodeUnprepareResources")?
            .into_inner();
        if let Some(result) = resp.claims.get(&claim.uid) {
            if !result.error.is_empty() {
                anyhow::bail!("driver '{driver}' failed to unprepare claim '{}': {}", claim.name, result.error);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
#[path = "dra_tests/claim_ref.rs"]
mod tests_claim_ref;
