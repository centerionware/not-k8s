//! Minimal CSI Node-service client for mounting `PersistentVolumeClaim`
//! volumes — the biggest remaining feature gap before this round (pods
//! couldn't reference a PVC at all).
//!
//! **Scoped as a first slice, not full CSI support.** Real kubelet
//! discovers CSI drivers dynamically: a driver's DaemonSet writes its
//! Node-service socket into `/var/lib/kubelet/plugins/<driver>/` and calls
//! kubelet's own plugin-registration gRPC service
//! (`pluginregistration.proto`) to announce itself. Implementing that
//! second gRPC server is a real chunk of additional CSI plumbing on top of
//! the Node service itself — this instead takes a simpler, still-real
//! approach: `NODELET_CSI_DRIVERS` statically maps a driver name to its
//! already-known Node-service socket path. A driver's own DaemonSet
//! container still runs and still serves that socket the exact same way
//! regardless of whether anything ever registers against it — the
//! registration dance is how kubelet *discovers* the socket path, not
//! something the Node-service RPCs themselves depend on. This works for
//! any CSI driver whose socket path is fixed/predictable (nearly all of
//! them — it's normally a well-known path baked into the driver's own
//! DaemonSet manifest), at the cost of nodelet needing that path
//! configured up front instead of discovering it automatically.
//!
//! Also out of scope for this slice, each documented at its point of use
//! below: dynamic provisioning (external-provisioner's job, not kubelet's)
//! and calling the Controller service's `ControllerPublishVolume`/
//! `ControllerUnpublishVolume` RPCs directly (that's external-attacher's
//! job too — real kubelet never calls them either). What real kubelet
//! *does* do on the node side of attach, and what `runtime/cri.rs`'s
//! `resolve_csi_source()` now does too: check whether the driver even
//! requires an attach (`CSIDriver.spec.attachRequired`), and if so, wait
//! for the `VolumeAttachment` object the external-attacher produces to
//! reach `status.attached == true` before staging/publishing, threading
//! its `status.attachmentMetadata` through as `publish_context`.

use anyhow::{Context, Result};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;
use std::sync::Mutex;
use tonic::transport::{Channel, Endpoint, Uri};
use tracing::{debug, warn};

/// Generated CSI v1 types and gRPC client (from proto/csi.proto, the
/// upstream container-storage-interface/spec repo's stable v1 API).
pub mod v1 {
    #![allow(clippy::doc_lazy_continuation)]
    tonic::include_proto!("csi.v1");
}

use v1::node_client::NodeClient;
use v1::volume_capability::{access_mode, AccessMode, AccessType, BlockVolume, MountVolume};
use v1::{
    NodeGetCapabilitiesRequest, NodeGetInfoRequest, NodeGetInfoResponse, NodePublishVolumeRequest,
    NodeStageVolumeRequest, NodeUnpublishVolumeRequest, NodeUnstageVolumeRequest,
    NodeServiceCapability, VolumeCapability,
};

/// Dial a CSI driver's Node-service Unix socket — same connector shape as
/// `runtime/cri.rs::connect_uds`, just a separate copy: CSI and CRI both
/// speak plain-unix-socket gRPC, but are otherwise independent proto
/// packages/connections with no reason to share a module.
async fn connect_uds(endpoint: &str) -> Result<Channel> {
    let path = endpoint.strip_prefix("unix://").unwrap_or(endpoint).to_string();
    let channel = Endpoint::try_from("http://localhost")
        .context("invalid endpoint")?
        .connect_with_connector(tower::service_fn(move |_: Uri| {
            let path = path.clone();
            async move {
                let stream = tokio::net::UnixStream::connect(path).await?;
                Ok::<_, std::io::Error>(hyper_util::rt::TokioIo::new(stream))
            }
        }))
        .await
        .context("connecting to CSI driver unix socket")?;
    Ok(channel)
}

/// Whether `capabilities` (from `NodeGetCapabilitiesResponse`) includes
/// `STAGE_UNSTAGE_VOLUME` — pulled out as a pure function so the decision
/// logic is unit-testable without a real CSI driver socket.
fn has_stage_unstage_capability(capabilities: &[NodeServiceCapability]) -> bool {
    capabilities.iter().any(|c| {
        matches!(
            &c.r#type,
            Some(v1::node_service_capability::Type::Rpc(rpc))
                if rpc.r#type == v1::node_service_capability::rpc::Type::StageUnstageVolume as i32
        )
    })
}

/// Where per-volume Stage mounts live — one per (driver, volume), shared
/// across every pod on this node that references the same
/// `PersistentVolume`, matching real kubelet's global staging directory
/// convention (`/var/lib/kubelet/plugins/kubernetes.io/csi/...`).
fn staging_path(driver: &str, volume_handle: &str) -> std::path::PathBuf {
    std::path::PathBuf::from("/var/lib/nodelet/csi")
        .join(driver)
        .join(volume_handle)
        .join("globalmount")
}

/// (driver, volume_handle) as written to/read from disk, keyed by
/// target_path — see `CsiDrivers::mounted`'s doc comment for why this
/// exists on disk at all, not just in memory.
#[derive(serde::Serialize, serde::Deserialize)]
struct MountMeta {
    driver: String,
    volume_handle: String,
}

/// Where `mount()`/`unmount()` persist a target_path's `MountMeta` — a
/// hidden sibling file next to target_path itself, never inside it (a
/// regular volume's target_path IS the mounted directory; a block volume's
/// is a bind-mounted device-node file — writing into either would corrupt
/// the mount, not just the metadata).
fn mount_meta_path(target_path: &Path) -> std::path::PathBuf {
    let mut file_name = std::ffi::OsString::from(".");
    file_name.push(target_path.file_name().unwrap_or_default());
    file_name.push(".csi-meta.json");
    target_path.with_file_name(file_name)
}

/// `block`: raw block volumes (round 77; `volumeMode: Block`, found in
/// round 76's re-audit) skip the filesystem entirely — `AccessType::Block`
/// instead of `Mount`, and `fs_type` is meaningless for it (the CSI spec
/// itself doesn't define a `fs_type` for block volumes).
fn mount_capability(fs_type: &str, read_only: bool, block: bool) -> VolumeCapability {
    let mode = if read_only { access_mode::Mode::MultiNodeReaderOnly } else { access_mode::Mode::SingleNodeWriter };
    let access_type = if block {
        AccessType::Block(BlockVolume {})
    } else {
        AccessType::Mount(MountVolume { fs_type: fs_type.to_string(), mount_flags: Vec::new(), volume_mount_group: String::new() })
    };
    VolumeCapability { access_mode: Some(AccessMode { mode: mode as i32 }), access_type: Some(access_type) }
}

/// One CSI volume actually in use — enough to unpublish/unstage it again
/// Everything needed to mount a `PersistentVolumeClaim` volume, resolved
/// from the bound `PersistentVolume`'s `.spec.csi` — pulled out as its own
/// type so `runtime/cri.rs`'s PVC resolution and this module's mount/unmount
/// logic aren't coupled to each other's internals.
pub struct CsiVolumeSource {
    pub driver: String,
    pub volume_handle: String,
    pub fs_type: String,
    pub read_only: bool,
    pub volume_attributes: HashMap<String, String>,
    /// From `.spec.csi.nodeStageSecretRef`, resolved to key/value pairs —
    /// empty if unset or the driver doesn't need one.
    pub node_stage_secrets: HashMap<String, String>,
    /// From `.spec.csi.nodePublishSecretRef`.
    pub node_publish_secrets: HashMap<String, String>,
    /// Opaque driver-specific data returned by `ControllerPublishVolume`
    /// (the external-attacher's job, not nodelet's — see the module doc
    /// comment) and stashed on the matching `VolumeAttachment.status`.
    /// Required by some drivers' NodeStage/NodePublish calls (e.g. a device
    /// path chosen at attach time). Empty for volumes that didn't need an
    /// attach at all (`CSIDriver.spec.attachRequired == false`, the common
    /// case for node-local/edge storage) — never populated in that case
    /// since there's no VolumeAttachment to read it from.
    pub publish_context: HashMap<String, String>,
    /// Round 77 (found in round 76's re-audit): `PersistentVolume.spec.volumeMode
    /// == "Block"` — the volume is published as a raw block device node
    /// rather than a mounted filesystem. `target_path` for a block volume
    /// must be a bind-mount-target *file*, not a directory (the CSI spec's
    /// own convention every driver expects), so `mount()` branches on this.
    pub block: bool,
}

pub struct CsiDrivers {
    /// driver name -> Node-service unix socket endpoint. Seeded from
    /// `NODELET_CSI_DRIVERS` at startup, and kept up to date afterwards by
    /// `plugin_registry.rs`'s dynamic registration watcher (a driver whose
    /// DaemonSet registers itself overrides/adds to the static config; one
    /// that deregisters or whose socket disappears is removed again). A
    /// `Mutex`, not a plain map, precisely because it's live-updated —
    /// empty (the default, before any driver has registered) means every
    /// PVC volume is skipped with a warning, same treatment any other
    /// unresolvable volume already gets.
    endpoints: Mutex<BTreeMap<String, String>>,
    /// Per-driver `STAGE_UNSTAGE_VOLUME` capability, fetched once and
    /// cached — real kubelet does the same rather than calling
    /// `NodeGetCapabilities` on every single mount.
    stage_capable: Mutex<HashMap<String, bool>>,
    /// (driver, volume_handle) -> the set of pod UIDs on this node
    /// currently using it. A *set*, not a plain counter — `ensure_pod()`
    /// (and so this module's `mount()`) is called on every reconcile of an
    /// already-running pod, not just once at creation, so a counter would
    /// inflate without bound; a set makes repeated calls for the same pod
    /// a no-op instead. `NodeUnstageVolume` only fires once the set is
    /// empty — matches real kubelet's own volume-manager reference
    /// counting. Purely in-memory: a nodelet restart loses it, but it
    /// self-heals since every still-running pod's next reconcile calls
    /// `mount()` again.
    refs: Mutex<HashMap<(String, String), HashSet<String>>>,
    /// target_path -> (driver, volume_handle) for every volume currently
    /// published via `mount()` — an in-memory hot cache in front of the
    /// on-disk `MountMeta` sidecar `mount()`/`unmount()` also
    /// write/remove next to each target_path (`mount_meta_path()`).
    /// Round 124 (found live in CI): before either existed,
    /// `unmount_csi_volumes()` (volumes_resolve.rs) had no way to know
    /// which driver/volume_handle a target_path belonged to except by
    /// re-fetching the owning PersistentVolumeClaim from the API server at
    /// teardown time — and a PVC deleted before or during teardown (a
    /// completely ordinary sequence: pod deleted, then its PVC) made that
    /// fetch 404, permanently abandoning the volume with no retry (leaked
    /// host mount, and a phantom entry stuck in Node.status.volumesInUse
    /// forever, since `mounted_volumes()` below reads straight from `refs`,
    /// which `unmount()` never got the chance to clear). The on-disk sidecar
    /// (not just this in-memory map) matters specifically because the mount
    /// itself survives a nodelet restart, so the metadata needed to unwind
    /// it later must too — an in-memory-only cache would silently reopen
    /// the exact same gap for any pod torn down after a restart. Real
    /// kubelet's own volume reconciler works the same way: off its own
    /// actual-state-of-world, not a live PVC re-fetch.
    mounted: Mutex<HashMap<String, (String, String)>>,
}

impl CsiDrivers {
    pub fn new(endpoints: BTreeMap<String, String>) -> Self {
        Self {
            endpoints: Mutex::new(endpoints),
            stage_capable: Mutex::new(HashMap::new()),
            refs: Mutex::new(HashMap::new()),
            mounted: Mutex::new(HashMap::new()),
        }
    }

    /// Look up the (driver, volume_handle) a target_path was published with,
    /// if `mount()` recorded it and nothing has unmounted it since. See the
    /// `mounted` field's own doc comment for why this exists. Checks the
    /// in-memory map first, falling back to the on-disk `MountMeta` sidecar
    /// `mount()` also writes — the in-memory map alone doesn't survive a
    /// nodelet restart, but the mount itself (and its sidecar file) does,
    /// so a pod torn down after a restart must still be able to recover
    /// this rather than falling all the way back to re-fetching a PVC that
    /// may already be gone. A disk hit backfills the in-memory map too, so
    /// this only touches disk once per target_path per process lifetime.
    pub fn mounted_source_for(&self, target_path: &Path) -> Option<(String, String)> {
        let key = target_path.to_string_lossy().into_owned();
        if let Some(hit) = self.mounted.lock().unwrap().get(&key).cloned() {
            return Some(hit);
        }
        let meta: MountMeta = std::fs::read_to_string(mount_meta_path(target_path)).ok().and_then(|s| serde_json::from_str(&s).ok())?;
        let value = (meta.driver, meta.volume_handle);
        self.mounted.lock().unwrap().insert(key, value.clone());
        Some(value)
    }

    pub fn driver_configured(&self, driver: &str) -> bool {
        self.endpoints.lock().unwrap().contains_key(driver)
    }

    /// Every `(driver, volume_handle)` pair currently mounted by at least
    /// one pod on this node (round 34) — feeds
    /// `Node.status.volumesInUse`/`.volumesAttached`. Real kubelet tracks
    /// this via its own volume manager's actual-state-of-world; nodelet
    /// already has the same information here via `mount()`/`unmount()`'s
    /// existing per-pod reference counting (`refs` only ever holds a key
    /// while at least one pod still references it — see `unmount()`), so
    /// this just exposes it rather than tracking it a second time.
    pub fn mounted_volumes(&self) -> Vec<(String, String)> {
        self.refs.lock().unwrap().keys().cloned().collect()
    }

    /// Add (or update, if the driver re-registers with a new endpoint) a
    /// dynamically-discovered driver. Called by `plugin_registry.rs` when a
    /// CSI driver's registrar announces itself.
    pub fn register(&self, driver: String, endpoint: String) {
        self.endpoints.lock().unwrap().insert(driver, endpoint);
    }

    /// Remove a driver — its registration socket disappeared, so its
    /// Node-service socket should no longer be assumed reachable either.
    /// Also drops any cached capability for it, so a re-registration under
    /// the same name gets a fresh `NodeGetCapabilities` call rather than a
    /// stale cached answer.
    pub fn deregister(&self, driver: &str) {
        self.endpoints.lock().unwrap().remove(driver);
        self.stage_capable.lock().unwrap().remove(driver);
    }

    fn endpoint_for(&self, driver: &str) -> Result<String> {
        self.endpoints
            .lock()
            .unwrap()
            .get(driver)
            .cloned()
            .with_context(|| format!("no CSI driver configured for '{driver}' — set NODELET_CSI_DRIVERS or wait for it to register"))
    }

    async fn client_for(&self, driver: &str) -> Result<NodeClient<Channel>> {
        let endpoint = self.endpoint_for(driver)?;
        let channel = connect_uds(&endpoint).await?;
        Ok(NodeClient::new(channel))
    }

    async fn supports_stage_unstage(&self, driver: &str) -> bool {
        if let Some(&cached) = self.stage_capable.lock().unwrap().get(driver) {
            return cached;
        }
        let supported = self.query_stage_unstage_capability(driver).await.unwrap_or_else(|e| {
            warn!(driver, error = ?e, "CSI: NodeGetCapabilities failed; assuming no STAGE_UNSTAGE_VOLUME support");
            false
        });
        self.stage_capable.lock().unwrap().insert(driver.to_string(), supported);
        supported
    }

    async fn query_stage_unstage_capability(&self, driver: &str) -> Result<bool> {
        let mut client = self.client_for(driver).await?;
        let resp = client.node_get_capabilities(NodeGetCapabilitiesRequest {}).await.context("NodeGetCapabilities")?.into_inner();
        Ok(has_stage_unstage_capability(&resp.capabilities))
    }

    /// `NodeGetInfo` — called once, right after `plugin_registry.rs` learns
    /// a driver has registered, so its `CSINode.spec.drivers[]` entry can
    /// be reconciled (see `csi_node.rs`). Without this, a topology-aware
    /// external-provisioner (`--feature-gates=Topology=true`, this
    /// codebase's own bundled CSI driver deploy manifest included — the
    /// common default for real CSI drivers, not an edge case) permanently
    /// fails every `CreateVolume` with "no available topology found",
    /// because it walks every `CSINode` object looking for the requesting
    /// driver's entry and never finds one — found live testing a real CSI
    /// driver's PVC provisioning, after rounds 117-119 already got the pod
    /// itself running cleanly.
    pub(crate) async fn node_info(&self, driver: &str) -> Result<NodeGetInfoResponse> {
        let mut client = self.client_for(driver).await?;
        Ok(client.node_get_info(NodeGetInfoRequest {}).await.context("NodeGetInfo")?.into_inner())
    }

    /// Stage (if the driver supports it) and publish `source` at
    /// `target_path`, so it's a real, populated mountpoint ready to be
    /// bind-mounted into a container the same way every other volume kind
    /// already is. Idempotent per the CSI spec — safe to call again for a
    /// pod that already has this volume mounted (e.g. every reconcile).
    /// `ephemeral` (round 46) — `true` for a CSI *ephemeral inline* volume
    /// (`volumes[].csi` specified directly, no PV/PVC at all): the CSI spec
    /// itself says ephemeral inline volumes never go through
    /// `NodeStageVolume`/`NodeUnstageVolume`, regardless of whether the
    /// driver otherwise reports that capability — there's no
    /// `VolumeAttachment`/central attach-detach concept for them either,
    /// only a direct `NodePublishVolume`.
    pub async fn mount(&self, source: &CsiVolumeSource, target_path: &Path, pod_uid: &str, ephemeral: bool) -> Result<()> {
        let key = (source.driver.clone(), source.volume_handle.clone());
        {
            let mut refs = self.refs.lock().unwrap();
            refs.entry(key.clone()).or_default().insert(pod_uid.to_string());
        }

        let mut client = self.client_for(&source.driver).await?;
        let capability = mount_capability(&source.fs_type, source.read_only, source.block);
        let volume_context: std::collections::HashMap<String, String> = source.volume_attributes.clone();

        let staging = staging_path(&source.driver, &source.volume_handle);
        let mut staging_target_path = String::new();
        if !ephemeral && self.supports_stage_unstage(&source.driver).await {
            std::fs::create_dir_all(&staging).context("creating CSI staging directory")?;
            staging_target_path = staging.to_string_lossy().into_owned();
            client
                .node_stage_volume(NodeStageVolumeRequest {
                    volume_id: source.volume_handle.clone(),
                    publish_context: source.publish_context.clone(),
                    staging_target_path: staging_target_path.clone(),
                    volume_capability: Some(capability.clone()),
                    secrets: source.node_stage_secrets.clone(),
                    volume_context: volume_context.clone(),
                })
                .await
                .context("NodeStageVolume")?;
        }

        // A block volume's bind-mount target must be an existing plain
        // FILE (the driver bind-mounts the real block device node onto
        // it) — every other volume kind's target is a directory.
        if source.block {
            if let Some(parent) = target_path.parent() {
                std::fs::create_dir_all(parent).context("creating CSI publish target's parent directory")?;
            }
            if !target_path.exists() {
                std::fs::File::create(target_path).context("creating CSI block volume publish target file")?;
            }
        } else {
            std::fs::create_dir_all(target_path).context("creating CSI publish target directory")?;
        }
        client
            .node_publish_volume(NodePublishVolumeRequest {
                volume_id: source.volume_handle.clone(),
                publish_context: source.publish_context.clone(),
                staging_target_path,
                target_path: target_path.to_string_lossy().into_owned(),
                volume_capability: Some(capability),
                readonly: source.read_only,
                secrets: source.node_publish_secrets.clone(),
                volume_context,
            })
            .await
            .context("NodePublishVolume")?;

        self.mounted
            .lock()
            .unwrap()
            .insert(target_path.to_string_lossy().into_owned(), (source.driver.clone(), source.volume_handle.clone()));
        let meta = MountMeta { driver: source.driver.clone(), volume_handle: source.volume_handle.clone() };
        if let Ok(json) = serde_json::to_string(&meta) {
            if let Err(e) = std::fs::write(mount_meta_path(target_path), json) {
                warn!(target_path = %target_path.display(), error = ?e, "CSI: failed to persist mount metadata sidecar — teardown after a nodelet restart may have to fall back to re-resolving the PVC");
            }
        }

        Ok(())
    }

    /// Unpublish `target_path`, then unstage the volume too if this was the
    /// last pod on this node referencing it. Best-effort: callers (pod
    /// removal) shouldn't fail the whole teardown over a CSI driver error —
    /// same treatment `graceful_stop_containers` already gives a failing
    /// `preStop` hook.
    ///
    /// Round 124 (found live in CI): `pods.rs`'s `teardown()` deliberately
    /// calls this twice per pod deletion by design — once when
    /// `deletionTimestamp` is first set, once more on the real
    /// `Event::Delete` once the object actually leaves etcd, specifically
    /// to tolerate "someone already finished this" as a harmless no-op.
    /// The CSI spec says NodeUnpublishVolume/NodeUnstageVolume SHOULD
    /// already be idempotent (a no-op success on a target that's already
    /// unpublished/unstaged), but the reference driver this project tests
    /// against instead returns a hard `NotFound` gRPC error on that
    /// expected second call. Previously that error propagated via `?`
    /// *before* any of this function's own bookkeeping (`refs`/`mounted`/
    /// the on-disk sidecar) ran — so the redundant second call, which is
    /// completely normal and expected, permanently prevented
    /// `mounted_volumes()` from ever clearing that entry, i.e. exactly the
    /// "Node.status.volumesInUse never clears" symptom this whole round
    /// was chasing, now traced to here rather than (only) the PVC-refetch
    /// gap fixed earlier. `NotFound` from either RPC now means "already
    /// done" and falls through to the same bookkeeping cleanup a real
    /// success would.
    pub async fn unmount(&self, driver: &str, volume_handle: &str, target_path: &Path, pod_uid: &str, ephemeral: bool) -> Result<()> {
        let mut client = self.client_for(driver).await?;
        match client
            .node_unpublish_volume(NodeUnpublishVolumeRequest {
                volume_id: volume_handle.to_string(),
                target_path: target_path.to_string_lossy().into_owned(),
            })
            .await
        {
            Ok(_) => {}
            Err(status) if status.code() == tonic::Code::NotFound => {
                debug!(driver, volume_handle, target_path = %target_path.display(), "NodeUnpublishVolume: driver reports this volume already gone — treating a redundant teardown call as already-done");
            }
            Err(status) => return Err(status).context("NodeUnpublishVolume"),
        }

        self.mounted.lock().unwrap().remove(&target_path.to_string_lossy().into_owned());
        let _ = std::fs::remove_file(mount_meta_path(target_path));

        let key = (driver.to_string(), volume_handle.to_string());
        let last_reference = {
            let mut refs = self.refs.lock().unwrap();
            if let Some(set) = refs.get_mut(&key) {
                set.remove(pod_uid);
                if set.is_empty() {
                    refs.remove(&key);
                    true
                } else {
                    false
                }
            } else {
                true // untracked (e.g. after a nodelet restart) — safe to assume last
            }
        };

        if last_reference && !ephemeral && self.supports_stage_unstage(driver).await {
            let staging = staging_path(driver, volume_handle);
            match client
                .node_unstage_volume(NodeUnstageVolumeRequest {
                    volume_id: volume_handle.to_string(),
                    staging_target_path: staging.to_string_lossy().into_owned(),
                })
                .await
            {
                Ok(_) => {}
                Err(status) if status.code() == tonic::Code::NotFound => {
                    debug!(driver, volume_handle, "NodeUnstageVolume: driver reports this volume already gone — treating a redundant teardown call as already-done");
                }
                Err(status) => return Err(status).context("NodeUnstageVolume"),
            }
            let _ = std::fs::remove_dir_all(&staging);
        }

        Ok(())
    }
}

#[cfg(test)]
#[path = "csi_tests/has_stage_unstage_capability.rs"]
mod tests_has_stage_unstage_capability;
#[cfg(test)]
#[path = "csi_tests/staging_path.rs"]
mod tests_staging_path;
#[cfg(test)]
#[path = "csi_tests/mount_capability.rs"]
mod tests_mount_capability;
#[cfg(test)]
#[path = "csi_tests/dynamic_registration.rs"]
mod tests_dynamic_registration;
