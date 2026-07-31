//! Device plugins: the kubelet-side client for the Device Plugin API
//! (`k8s.io/kubelet/pkg/apis/deviceplugin/v1beta1`) — how a node advertises
//! and allocates specialized hardware (GPUs, FPGAs, and similar) that
//! isn't a first-class Kubernetes resource type. Real value for nodelet's
//! edge-device target: an edge box with an attached accelerator can now
//! actually schedule and run workloads that request it.
//!
//! **Reuses round 13's plugin-registration infrastructure directly** —
//! device plugins register through the exact same `GetInfo`/
//! `NotifyRegistrationStatus` handshake CSI drivers do (see
//! `plugin_registry.rs`), just with `PluginInfo.type == "DevicePlugin"`
//! instead of `"CSIPlugin"`. Once registered, this module dials the
//! plugin's own endpoint (from `PluginInfo.endpoint`, same delegation
//! model CSI uses) and keeps its `ListAndWatch` stream open for the
//! lifetime of the registration, tracking device health as it changes.
//!
//! Three responsibilities:
//! 1. **Inventory** (`ListAndWatch`) — per-resource-name device list +
//!    health, kept live via a background stream-reading task per plugin.
//! 2. **Capacity advertisement** (`capacity_map()`) — healthy device counts
//!    feed `Node.status.capacity`/`.allocatable` (see `node.rs`), so the
//!    scheduler knows this node can satisfy e.g. `nvidia.com/gpu: 1`.
//! 3. **Allocation** (`allocate()`) — called from `runtime/cri.rs` while
//!    building a container that requested the resource; picks specific
//!    healthy, not-already-allocated device IDs, calls the plugin's
//!    `Allocate` RPC, and returns the envs/mounts/device-nodes it says to
//!    inject — merged into the container's `ContainerConfig` the same way
//!    everything else already is.
//!
//! **Not implemented**: `GetPreferredAllocation` (this always does its own
//! "first N healthy, unallocated" selection — a real simplification vs.
//! real kubelet's topology-aware placement, but device-count correctness
//! doesn't depend on it) and `PreStartContainer` (some plugins want a
//! call right before each container start; skipped, matches this round's
//! "first slice" framing — a plugin requiring it will still register and
//! report devices, `Allocate` will still work, `PreStartContainer` is
//! simply never called).

use anyhow::{Context, Result};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tonic::transport::{Channel, Endpoint, Uri};
use tracing::{info, warn};

pub mod v1beta1 {
    tonic::include_proto!("v1beta1");
}

use v1beta1::device_plugin_client::DevicePluginClient;
use v1beta1::{AllocateRequest, ContainerAllocateRequest, ContainerAllocateResponse, Empty};

/// How long to wait before reconnecting a `ListAndWatch` stream that ended
/// or errored (the plugin process restarting is the common case) — a
/// plugin that's genuinely gone gets retried forever at this interval
/// until `deregister()` (its registration socket disappearing) stops it.
const RECONNECT_DELAY: Duration = Duration::from_secs(5);

/// Dial a device plugin's own Unix socket — same connector shape as every
/// other `connect_uds` in this codebase (`runtime/cri.rs`, `runtime/csi.rs`,
/// `plugin_registry.rs`), one more independent copy for the same reason
/// those already are: different proto package, not worth coupling modules
/// over a ~15-line helper.
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
        .context("connecting to device plugin unix socket")
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeviceInfo {
    pub id: String,
    pub healthy: bool,
}

/// Pure selection logic behind `DevicePlugins::allocate()`: the first
/// `count` healthy, not-already-allocated devices, in list order. `None`
/// if there aren't enough — pulled out specifically so this decision is
/// unit-testable without a live device plugin socket.
fn pick_devices(devices: &[DeviceInfo], allocated: &HashSet<String>, count: u64) -> Option<Vec<String>> {
    let picked: Vec<String> = devices.iter().filter(|d| d.healthy && !allocated.contains(&d.id)).take(count as usize).map(|d| d.id.clone()).collect();
    (picked.len() as u64 >= count).then_some(picked)
}

struct PluginState {
    endpoint: String,
    devices: Vec<DeviceInfo>,
    /// Device IDs currently allocated to a container — never handed out
    /// again by `allocate()` until `release()`'s called for them.
    allocated: HashSet<String>,
}

pub struct DevicePlugins {
    /// resource name (e.g. `nvidia.com/gpu`) -> plugin state.
    plugins: Mutex<HashMap<String, PluginState>>,
}

impl DevicePlugins {
    pub fn new() -> Self {
        Self { plugins: Mutex::new(HashMap::new()) }
    }

    /// Register a device plugin and start tracking its device inventory.
    /// Called by `plugin_registry.rs` when a `PluginInfo.type ==
    /// "DevicePlugin"` registration comes in. Spawns a background task
    /// that holds the plugin's `ListAndWatch` stream open for as long as
    /// this registration stays current.
    pub fn register(self: &Arc<Self>, resource_name: String, endpoint: String) {
        {
            let mut plugins = self.plugins.lock().unwrap();
            plugins.insert(resource_name.clone(), PluginState { endpoint: endpoint.clone(), devices: Vec::new(), allocated: HashSet::new() });
        }
        let this = self.clone();
        tokio::spawn(async move { watch_loop(this, resource_name, endpoint).await });
    }

    /// Deregister — its registration socket disappeared. The in-flight
    /// `ListAndWatch` task notices on its next reconnect attempt (via
    /// `is_current()`) and exits instead of resurrecting a stale entry.
    pub fn deregister(&self, resource_name: &str) {
        self.plugins.lock().unwrap().remove(resource_name);
    }

    pub fn resource_configured(&self, resource_name: &str) -> bool {
        self.plugins.lock().unwrap().contains_key(resource_name)
    }

    /// resource name -> count of currently healthy devices, for
    /// `Node.status.capacity`/`.allocatable` (see `node.rs`). Real kubelet
    /// doesn't subtract already-allocated devices from this — that
    /// accounting is the scheduler's job, based on what it's already bound
    /// to this node, same as cpu/memory.
    pub fn capacity_map(&self) -> BTreeMap<String, u64> {
        self.plugins
            .lock()
            .unwrap()
            .iter()
            .map(|(name, state)| (name.clone(), state.devices.iter().filter(|d| d.healthy).count() as u64))
            .collect()
    }

    fn update_devices(&self, resource_name: &str, endpoint: &str, new_devices: Vec<DeviceInfo>) -> bool {
        let mut plugins = self.plugins.lock().unwrap();
        match plugins.get_mut(resource_name) {
            Some(state) if state.endpoint == endpoint => {
                state.devices = new_devices;
                true
            }
            _ => false, // deregistered, or re-registered under a new endpoint — this watcher is stale
        }
    }

    fn is_current(&self, resource_name: &str, endpoint: &str) -> bool {
        self.plugins.lock().unwrap().get(resource_name).map(|s| s.endpoint == endpoint).unwrap_or(false)
    }

    /// Pick `count` healthy, currently-unallocated device IDs for
    /// `resource_name`, mark them allocated, and call the plugin's
    /// `Allocate` RPC for them. Returns the device IDs (so the caller can
    /// `release()` them later, e.g. on container teardown) alongside the
    /// plugin's response (envs/mounts/devices/annotations to inject).
    /// Devices are put back if anything after picking them fails — a
    /// failed allocation must not permanently strand devices as "in use."
    pub async fn allocate(&self, resource_name: &str, count: u64) -> Result<(Vec<String>, ContainerAllocateResponse)> {
        let (endpoint, device_ids) = {
            let mut plugins = self.plugins.lock().unwrap();
            let state = plugins.get_mut(resource_name).with_context(|| format!("no device plugin registered for '{resource_name}'"))?;
            let picked = pick_devices(&state.devices, &state.allocated, count)
                .with_context(|| format!("not enough healthy devices available for '{resource_name}'"))?;
            for id in &picked {
                state.allocated.insert(id.clone());
            }
            (state.endpoint.clone(), picked)
        };

        match self.allocate_call(&endpoint, &device_ids).await {
            Ok(resp) => Ok((device_ids, resp)),
            Err(e) => {
                self.release(resource_name, &device_ids);
                Err(e)
            }
        }
    }

    async fn allocate_call(&self, endpoint: &str, device_ids: &[String]) -> Result<ContainerAllocateResponse> {
        let channel = connect_uds(endpoint).await?;
        let mut client = DevicePluginClient::new(channel);
        let mut resp = client
            .allocate(AllocateRequest { container_requests: vec![ContainerAllocateRequest { devices_ids: device_ids.to_vec() }] })
            .await
            .context("Allocate")?
            .into_inner();
        if resp.container_responses.is_empty() {
            anyhow::bail!("device plugin Allocate() returned no container response");
        }
        Ok(resp.container_responses.remove(0))
    }

    /// Give back device IDs previously returned by `allocate()` — call on
    /// container teardown/restart so they're available for the next
    /// allocation. Silently a no-op for a resource whose plugin has since
    /// deregistered (nothing left to release into).
    pub fn release(&self, resource_name: &str, device_ids: &[String]) {
        if let Some(state) = self.plugins.lock().unwrap().get_mut(resource_name) {
            for id in device_ids {
                state.allocated.remove(id);
            }
        }
    }
}

async fn watch_loop(devices: Arc<DevicePlugins>, resource_name: String, endpoint: String) {
    loop {
        if !devices.is_current(&resource_name, &endpoint) {
            return; // deregistered (or replaced by a fresher registration) since this task started
        }
        if let Err(e) = watch_once(&devices, &resource_name, &endpoint).await {
            warn!(resource = %resource_name, error = ?e, "device plugin: ListAndWatch stream ended; will retry");
        }
        tokio::time::sleep(RECONNECT_DELAY).await;
    }
}

async fn watch_once(devices: &Arc<DevicePlugins>, resource_name: &str, endpoint: &str) -> Result<()> {
    let channel = connect_uds(endpoint).await?;
    let mut client = DevicePluginClient::new(channel);
    let mut stream = client.list_and_watch(Empty {}).await.context("ListAndWatch")?.into_inner();

    while let Some(resp) = stream.message().await.context("reading ListAndWatch stream")? {
        let list: Vec<DeviceInfo> = resp.devices.into_iter().map(|d| DeviceInfo { id: d.id, healthy: d.health == "Healthy" }).collect();
        info!(resource = %resource_name, devices = list.len(), healthy = list.iter().filter(|d| d.healthy).count(), "device plugin: inventory updated");
        if !devices.update_devices(resource_name, endpoint, list) {
            return Ok(()); // stale — deregistered or re-registered elsewhere; stop watching quietly
        }
    }
    Ok(())
}

#[cfg(test)]
#[path = "device_plugins_tests/pick_devices.rs"]
mod tests_pick_devices;
#[cfg(test)]
#[path = "device_plugins_tests/capacity_map.rs"]
mod tests_capacity_map;
#[cfg(test)]
#[path = "device_plugins_tests/registration_state.rs"]
mod tests_registration_state;
