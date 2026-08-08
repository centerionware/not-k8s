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
//! **`GetPreferredAllocation`/`PreStartContainer` (round 21)**: a plugin's
//! `DevicePluginOptions` (fetched once via `GetDevicePluginOptions` right
//! after registering, alongside its device inventory) says whether either
//! is needed. If `get_preferred_allocation_available`, `allocate_preferring()`
//! offers the plugin the full healthy-unallocated candidate list and lets
//! it pick — falling back to nodelet's own "first N, NUMA-preferring"
//! selection (`pick_devices_preferring()`) if the plugin's response is
//! missing, malformed, or (a race lost against a concurrent allocation)
//! no longer valid by the time it comes back. If `pre_start_required`,
//! `PreStartContainer` is called with the final device IDs right after
//! `Allocate()` succeeds and before the container actually starts,
//! matching upstream's own ordering — a failure here fails the whole
//! allocation (devices released) the same way an `Allocate()` failure
//! already does.

use anyhow::{Context, Result};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc::UnboundedSender;
use tonic::transport::{Channel, Endpoint, Uri};
use tracing::{info, warn};

pub mod v1beta1 {
    tonic::include_proto!("v1beta1");
}

use v1beta1::device_plugin_client::DevicePluginClient;
use v1beta1::{
    AllocateRequest, ContainerAllocateRequest, ContainerAllocateResponse, ContainerPreferredAllocationRequest, Empty,
    PreStartContainerRequest, PreferredAllocationRequest,
};

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
    /// The first NUMA node ID from the plugin's `TopologyInfo`, if it
    /// reported one — `None` means the plugin didn't report topology at
    /// all, which Topology Manager (`topology.rs`) treats as "compatible
    /// with every NUMA node," not "compatible with none." A device
    /// spanning multiple NUMA nodes only keeps the first; real kubelet's
    /// own hint generation does the same simplification.
    pub numa_node: Option<u32>,
}

/// Pure selection logic behind `DevicePlugins::allocate()`: the first
/// `count` healthy, not-already-allocated devices, in list order — pulled
/// out specifically so this decision is unit-testable without a live
/// device plugin socket. If `preferred_numa_node` is set (Topology
/// Manager's aligned-node choice — see `topology.rs`), devices on that
/// node are tried first, falling back to any other free device if the
/// preferred node alone can't supply `count`. A device with `numa_node:
/// None` (the plugin didn't report topology) counts toward *either* the
/// preferred-node pass or the fallback pass, matching
/// `topology::device_hint()`'s "compatible with every node" treatment of
/// untagged devices. `None` overall if there still aren't enough.
fn pick_devices_preferring(devices: &[DeviceInfo], allocated: &HashSet<String>, count: u64, preferred_numa_node: Option<u32>) -> Option<Vec<String>> {
    let free = || devices.iter().filter(|d| d.healthy && !allocated.contains(&d.id));

    let mut picked: Vec<String> = match preferred_numa_node {
        Some(node) => free().filter(|d| d.numa_node.is_none_or(|n| n == node)).take(count as usize).map(|d| d.id.clone()).collect(),
        None => Vec::new(),
    };
    if (picked.len() as u64) < count {
        for d in free() {
            if picked.len() as u64 >= count {
                break;
            }
            if !picked.contains(&d.id) {
                picked.push(d.id.clone());
            }
        }
    }
    (picked.len() as u64 >= count).then_some(picked)
}

/// Whether a `GetPreferredAllocation` response is safe to actually use —
/// pure so it's unit-testable without a live plugin. `ids` must be
/// exactly `count` device IDs, no duplicates, and every one of them
/// currently healthy and unallocated in `devices`/`allocated` — the same
/// checks a hand-rolled selection would already satisfy by construction,
/// but a plugin's response is untrusted input: it could return garbage
/// IDs, too few/many, duplicates, or (a real race, not just a hostile
/// plugin) IDs that got allocated to something else in the time between
/// this module snapshotting the candidate list and the plugin's response
/// coming back. Any of those falls back to `pick_devices_preferring()`
/// instead of trusting the plugin blindly.
fn is_valid_preferred_allocation(ids: &[String], devices: &[DeviceInfo], allocated: &HashSet<String>, count: u64) -> bool {
    if ids.len() as u64 != count {
        return false;
    }
    let unique: HashSet<&String> = ids.iter().collect();
    if unique.len() != ids.len() {
        return false;
    }
    ids.iter().all(|id| devices.iter().any(|d| &d.id == id && d.healthy) && !allocated.contains(id))
}

/// Same directory `runtime/cri/container_support.rs`'s
/// `device_alloc_checkpoint_path()` writes into — kept as a separate
/// literal here rather than importing that module's own constant, same
/// "self-contained module, shared filesystem contract, no cross-module
/// type coupling" precedent `csi.rs`'s own `MountMeta` sidecar already
/// set for CSI volumes.
const DEVICE_ALLOC_CHECKPOINT_DIR: &str = "/var/lib/nodelet/device-plugins/allocations";

/// The subset of `container_support.rs`'s `DeviceAllocMeta` fields this
/// module actually needs — deserializing the exact same on-disk JSON,
/// just ignoring `container_name`, which restoring `DevicePlugins`' own
/// `allocated`/`owners` state has no use for.
#[derive(serde::Deserialize)]
struct RestoredAllocation {
    resource_name: String,
    device_ids: Vec<String>,
    pod_key: String,
}

struct PluginState {
    endpoint: String,
    devices: Vec<DeviceInfo>,
    /// Device IDs currently allocated to a container — never handed out
    /// again by `allocate()` until `release()`'s called for them.
    allocated: HashSet<String>,
    /// From the plugin's `DevicePluginOptions`, fetched once via
    /// `GetDevicePluginOptions` right after registering (see
    /// `watch_once()`) — both default to `false` until that first call
    /// completes, matching this module's existing "not implemented"
    /// behavior for the brief window before it does.
    pre_start_required: bool,
    get_preferred_allocation_available: bool,
}

pub struct DevicePlugins {
    /// resource name (e.g. `nvidia.com/gpu`) -> plugin state.
    plugins: Mutex<HashMap<String, PluginState>>,
    /// `(resource_name, device_id)` -> the `"namespace/name"` pod key
    /// currently using it — round 124 (found live in CI): a device
    /// plugin's `ListAndWatch` reporting a device unhealthy is, like a
    /// probe transition, a real state change that never touches the Pod
    /// object itself. Without this, nothing would ever re-trigger a
    /// status write for the pod using that device — confirmed live:
    /// nodelet's own internal inventory correctly updated within
    /// seconds, but `containerStatuses[].allocatedResourcesStatus` on the
    /// pod itself never updated no matter how long a test waited, because
    /// nothing here had any way to know *which* pod to tell. Populated by
    /// `record_owner()` right after a successful `allocate_preferring()`
    /// (container_create.rs), cleared by `release()`.
    owners: Mutex<HashMap<(String, String), String>>,
    /// Poked with a pod key whenever an owned device's health changes —
    /// the exact same general-purpose "something changed outside a watch
    /// event, please re-sync this pod's status" channel `events_gc.rs`'s
    /// CRI event loop already feeds into `pods.rs`'s `on_runtime_event()`
    /// (see `CriRuntime::new()`, which clones its own `tx` into here).
    notify: UnboundedSender<String>,
}

impl DevicePlugins {
    pub fn new(notify: UnboundedSender<String>) -> Self {
        Self { plugins: Mutex::new(HashMap::new()), owners: Mutex::new(HashMap::new()), notify }
    }

    /// Record that `pod_key` ("namespace/name") is now using these
    /// `device_ids` of `resource_name` — see `owners`' own doc comment
    /// for why this exists. Called right after a successful
    /// `allocate_preferring()`.
    pub fn record_owner(&self, resource_name: &str, device_ids: &[String], pod_key: &str) {
        let mut owners = self.owners.lock().unwrap();
        for id in device_ids {
            owners.insert((resource_name.to_string(), id.clone()), pod_key.to_string());
        }
    }

    /// Register a device plugin and start tracking its device inventory.
    /// Called by `plugin_registry.rs` when a `PluginInfo.type ==
    /// "DevicePlugin"` registration comes in. Spawns a background task
    /// that holds the plugin's `ListAndWatch` stream open for as long as
    /// this registration stays current.
    pub fn register(self: &Arc<Self>, resource_name: String, endpoint: String) {
        {
            let mut plugins = self.plugins.lock().unwrap();
            plugins.insert(
                resource_name.clone(),
                PluginState {
                    endpoint: endpoint.clone(),
                    devices: Vec::new(),
                    allocated: HashSet::new(),
                    pre_start_required: false,
                    get_preferred_allocation_available: false,
                },
            );
        }
        // Round 124: restore this resource's own already-allocated
        // devices from disk BEFORE this plugin can possibly serve any new
        // Allocate() call — see restore_allocations_from_disk()'s own doc
        // comment for why a nodelet restart otherwise makes an
        // already-in-use device look free again, letting it get
        // double-booked onto a second, unrelated container.
        self.restore_allocations_from_disk(&resource_name);
        let this = self.clone();
        tokio::spawn(async move { watch_loop(this, resource_name, endpoint).await });
    }

    /// Scan the on-disk allocation checkpoints container_support.rs'
    /// `record_device_allocations()` writes (same directory/JSON shape,
    /// duck-typed — this struct only reads the fields it needs, no
    /// cross-module type coupling, matching how `csi.rs`'s `MountMeta` is
    /// self-contained too) for anything belonging to `resource_name`, and
    /// mark those specific device IDs allocated + owned again. Purely
    /// local bookkeeping — no RPC to the plugin, since these devices were
    /// already really allocated by a container that's (presumably) still
    /// running; this just makes nodelet's own view of the world catch up
    /// after losing it to a restart. Best-effort: a device whose
    /// checkpoint claims it but that this fresh inventory later reports
    /// as unhealthy/gone just won't show up as allocated once
    /// `update_devices()` overwrites `state.devices` — not this
    /// function's problem to solve.
    fn restore_allocations_from_disk(&self, resource_name: &str) {
        let Ok(entries) = std::fs::read_dir(DEVICE_ALLOC_CHECKPOINT_DIR) else { return };
        for entry in entries.flatten() {
            let Ok(content) = std::fs::read_to_string(entry.path()) else { continue };
            let Ok(meta) = serde_json::from_str::<RestoredAllocation>(&content) else { continue };
            if meta.resource_name != resource_name {
                continue;
            }
            if let Some(state) = self.plugins.lock().unwrap().get_mut(resource_name) {
                for id in &meta.device_ids {
                    state.allocated.insert(id.clone());
                }
            }
            let mut owners = self.owners.lock().unwrap();
            for id in meta.device_ids {
                owners.insert((resource_name.to_string(), id), meta.pod_key.clone());
            }
        }
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

    /// The NUMA-node affinity of every currently healthy, unallocated
    /// device for `resource_name` — what Topology Manager's
    /// `topology::device_hint()` needs to decide which NUMA nodes can
    /// satisfy a request for this resource. Empty for an unconfigured
    /// resource (matches every other "unknown resource" case in this
    /// module — the caller learns that from `resource_configured()`
    /// separately, this just has nothing to report).
    pub fn available_device_numa_nodes(&self, resource_name: &str) -> Vec<Option<u32>> {
        self.plugins
            .lock()
            .unwrap()
            .get(resource_name)
            .map(|state| state.devices.iter().filter(|d| d.healthy && !state.allocated.contains(&d.id)).map(|d| d.numa_node).collect())
            .unwrap_or_default()
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

    /// Live health for one already-allocated device — `containerStatuses[].allocatedResourcesStatus`
    /// (round 79; `ResourceHealthStatus`, KEP-4680, found in round 72's
    /// re-audit) needs this per-device signal for devices a container is
    /// *currently* using, not just the aggregate healthy-count
    /// `capacity_map()` reports. `None` if `resource_name` isn't
    /// registered at all (e.g. the plugin deregistered since allocation —
    /// matches upstream's own `Unknown` case) or `device_id` isn't in its
    /// current inventory; `Some(healthy)` otherwise, straight off the
    /// same `ListAndWatch`-fed state `capacity_map()`/`allocate()` already
    /// use — no new tracking, purely a read of what's already there.
    pub fn health_of(&self, resource_name: &str, device_id: &str) -> Option<bool> {
        self.plugins.lock().unwrap().get(resource_name)?.devices.iter().find(|d| d.id == device_id).map(|d| d.healthy)
    }

    /// `(resource_name, healthy device IDs)` for every resource — the
    /// PodResources API's `GetAllocatableResources` (round 74) needs the
    /// actual IDs, not just `capacity_map()`'s counts.
    pub fn all_healthy_device_ids(&self) -> Vec<(String, Vec<String>)> {
        self.plugins
            .lock()
            .unwrap()
            .iter()
            .map(|(name, state)| (name.clone(), state.devices.iter().filter(|d| d.healthy).map(|d| d.id.clone()).collect()))
            .collect()
    }

    fn update_devices(&self, resource_name: &str, endpoint: &str, new_devices: Vec<DeviceInfo>) -> bool {
        // Round 124: notify whichever pod(s) own a device whose health
        // just flipped — see `owners`' own doc comment for why. Diffed
        // against the OLD device list before it's overwritten below. This
        // is the only place that holds both `plugins` and `owners` at
        // once (nested, plugins outer); every other method here only
        // ever holds one at a time (never nested), so there's no cycle
        // for these two locks to deadlock on.
        let changed_owners: Vec<String> = {
            let mut plugins = self.plugins.lock().unwrap();
            match plugins.get_mut(resource_name) {
                Some(state) if state.endpoint == endpoint => {
                    let old_health: HashMap<&str, bool> = state.devices.iter().map(|d| (d.id.as_str(), d.healthy)).collect();
                    let owners = self.owners.lock().unwrap();
                    let changed = new_devices
                        .iter()
                        .filter(|d| old_health.get(d.id.as_str()).is_some_and(|&was_healthy| was_healthy != d.healthy))
                        .filter_map(|d| owners.get(&(resource_name.to_string(), d.id.clone())).cloned())
                        .collect();
                    state.devices = new_devices;
                    changed
                }
                _ => return false, // deregistered, or re-registered under a new endpoint — this watcher is stale
            }
        };
        for pod_key in changed_owners {
            let _ = self.notify.send(pod_key);
        }
        true
    }

    fn is_current(&self, resource_name: &str, endpoint: &str) -> bool {
        self.plugins.lock().unwrap().get(resource_name).map(|s| s.endpoint == endpoint).unwrap_or(false)
    }

    /// Record a plugin's `DevicePluginOptions`, fetched once right after
    /// registration (see `watch_once()`). Same staleness guard as
    /// `update_devices()`: a `false` return means this watcher's
    /// registration has already been superseded, so the caller should
    /// stop rather than write into a fresher registration's state.
    fn set_options(&self, resource_name: &str, endpoint: &str, pre_start_required: bool, get_preferred_allocation_available: bool) -> bool {
        let mut plugins = self.plugins.lock().unwrap();
        match plugins.get_mut(resource_name) {
            Some(state) if state.endpoint == endpoint => {
                state.pre_start_required = pre_start_required;
                state.get_preferred_allocation_available = get_preferred_allocation_available;
                true
            }
            _ => false,
        }
    }

    /// Pick `count` healthy, currently-unallocated device IDs for
    /// `resource_name`, mark them allocated, and call the plugin's
    /// `Allocate` RPC for them. Returns the device IDs (so the caller can
    /// `release()` them later, e.g. on container teardown) alongside the
    /// plugin's response (envs/mounts/devices/annotations to inject).
    /// Devices are put back if anything after picking them fails — a
    /// failed allocation must not permanently strand devices as "in use."
    pub async fn allocate(&self, resource_name: &str, count: u64) -> Result<(Vec<String>, ContainerAllocateResponse)> {
        self.allocate_preferring(resource_name, count, None).await
    }

    /// Same as `allocate()`, but tries devices on `preferred_numa_node`
    /// first — Topology Manager's aligned-node choice, so this container's
    /// devices land on the same NUMA node its exclusive CPUs (if any) do.
    ///
    /// If the plugin supports `GetPreferredAllocation` (round 21), it gets
    /// first say: offered the full healthy-unallocated candidate list, its
    /// response used verbatim if `is_valid_preferred_allocation()` accepts
    /// it, falling back to nodelet's own `pick_devices_preferring()`
    /// otherwise (missing/malformed response, or a lost race against a
    /// concurrent allocation). If the plugin requires `PreStartContainer`,
    /// it's called with the final device IDs right after `Allocate()`
    /// succeeds — a failure there releases the devices and fails the
    /// allocation, same treatment an `Allocate()` failure already gets.
    pub async fn allocate_preferring(
        &self,
        resource_name: &str,
        count: u64,
        preferred_numa_node: Option<u32>,
    ) -> Result<(Vec<String>, ContainerAllocateResponse)> {
        let (endpoint, available_ids, get_preferred_allocation_available, pre_start_required) = {
            let plugins = self.plugins.lock().unwrap();
            let state = plugins.get(resource_name).with_context(|| format!("no device plugin registered for '{resource_name}'"))?;
            let available_ids: Vec<String> =
                state.devices.iter().filter(|d| d.healthy && !state.allocated.contains(&d.id)).map(|d| d.id.clone()).collect();
            (state.endpoint.clone(), available_ids, state.get_preferred_allocation_available, state.pre_start_required)
        };

        let preferred_ids = if get_preferred_allocation_available {
            match self.get_preferred_allocation_call(&endpoint, &available_ids, count).await {
                Ok(ids) => Some(ids),
                Err(e) => {
                    warn!(resource = %resource_name, error = ?e, "device plugin GetPreferredAllocation failed; falling back to nodelet's own selection");
                    None
                }
            }
        } else {
            None
        };

        let device_ids = {
            let mut plugins = self.plugins.lock().unwrap();
            let state = plugins.get_mut(resource_name).with_context(|| format!("no device plugin registered for '{resource_name}'"))?;
            let picked = match &preferred_ids {
                Some(ids) if is_valid_preferred_allocation(ids, &state.devices, &state.allocated, count) => ids.clone(),
                _ => pick_devices_preferring(&state.devices, &state.allocated, count, preferred_numa_node)
                    .with_context(|| format!("not enough healthy devices available for '{resource_name}'"))?,
            };
            for id in &picked {
                state.allocated.insert(id.clone());
            }
            picked
        };

        match self.allocate_call(&endpoint, &device_ids).await {
            Ok(resp) if pre_start_required => match self.pre_start_container_call(&endpoint, &device_ids).await {
                Ok(()) => Ok((device_ids, resp)),
                Err(e) => {
                    self.release(resource_name, &device_ids);
                    Err(e)
                }
            },
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

    async fn get_preferred_allocation_call(&self, endpoint: &str, available_ids: &[String], count: u64) -> Result<Vec<String>> {
        let channel = connect_uds(endpoint).await?;
        let mut client = DevicePluginClient::new(channel);
        let mut resp = client
            .get_preferred_allocation(PreferredAllocationRequest {
                container_requests: vec![ContainerPreferredAllocationRequest {
                    available_device_i_ds: available_ids.to_vec(),
                    must_include_device_i_ds: Vec::new(),
                    allocation_size: count as i32,
                }],
            })
            .await
            .context("GetPreferredAllocation")?
            .into_inner();
        if resp.container_responses.is_empty() {
            anyhow::bail!("device plugin GetPreferredAllocation returned no container response");
        }
        Ok(resp.container_responses.remove(0).device_i_ds)
    }

    async fn pre_start_container_call(&self, endpoint: &str, device_ids: &[String]) -> Result<()> {
        let channel = connect_uds(endpoint).await?;
        let mut client = DevicePluginClient::new(channel);
        client.pre_start_container(PreStartContainerRequest { devices_ids: device_ids.to_vec() }).await.context("PreStartContainer")?;
        Ok(())
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
        let mut owners = self.owners.lock().unwrap();
        for id in device_ids {
            owners.remove(&(resource_name.to_string(), id.clone()));
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

    let options = client.get_device_plugin_options(Empty {}).await.context("GetDevicePluginOptions")?.into_inner();
    if !devices.set_options(resource_name, endpoint, options.pre_start_required, options.get_preferred_allocation_available) {
        return Ok(()); // stale — deregistered or re-registered elsewhere since this task started
    }

    let mut stream = client.list_and_watch(Empty {}).await.context("ListAndWatch")?.into_inner();

    while let Some(resp) = stream.message().await.context("reading ListAndWatch stream")? {
        let list: Vec<DeviceInfo> = resp
            .devices
            .into_iter()
            .map(|d| DeviceInfo {
                id: d.id,
                healthy: d.health == "Healthy",
                numa_node: d.topology.as_ref().and_then(|t| t.nodes.first()).map(|n| n.id as u32),
            })
            .collect();
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
#[cfg(test)]
#[path = "device_plugins_tests/preferred_allocation.rs"]
mod tests_preferred_allocation;
#[cfg(test)]
#[path = "device_plugins_tests/all_healthy_device_ids.rs"]
mod tests_all_healthy_device_ids;
#[cfg(test)]
#[path = "device_plugins_tests/health_of.rs"]
mod tests_health_of;
