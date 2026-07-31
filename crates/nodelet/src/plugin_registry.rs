//! The client-side half of the CSI/DevicePlugin plugin-registration
//! protocol — closes round 12's biggest flagged gap: `runtime/csi.rs`
//! originally only knew about CSI drivers via a static
//! `NODELET_CSI_DRIVERS` config, meaning an unmodified, off-the-shelf CSI
//! driver Helm chart (which expects to *announce itself* to whatever's
//! running at kubelet's plugin-registry socket directory, not be
//! hand-configured) wouldn't actually get discovered.
//!
//! The protocol is inverted from what the name suggests: the *plugin*
//! (specifically, its `node-driver-registrar` sidecar for CSI) runs the
//! gRPC **server**, on a socket it creates inside a shared, watched
//! directory. Kubelet — and here, nodelet — is the **client**: it watches
//! that directory for new sockets, dials each one, and calls `GetInfo()`
//! to learn the plugin's name/type/endpoint, then `NotifyRegistrationStatus()`
//! to confirm (or reject) the registration.
//!
//! Directory watching is poll-based (matches the rest of this codebase's
//! style — static_pods.rs, log rotation — over pulling in a filesystem
//! notification dependency for something that only needs to react within a
//! few seconds, not instantly).
//!
//! Handles all 3 `PluginInfo.type` values real kubelet's plugin watcher
//! does: `"CSIPlugin"` routes to `runtime::csi::CsiDrivers` (round 12/13),
//! `"DevicePlugin"` routes to `device_plugins::DevicePlugins` (round 14),
//! `"DRAPlugin"` routes to `dra::DraDrivers` (round 63). Anything else
//! gets a real `NotifyRegistrationStatus{plugin_registered: false, ...}`
//! rather than being silently ignored — the plugin gets an answer either
//! way, matching what a registrar sidecar actually expects to receive.

use crate::device_plugins::DevicePlugins;
use crate::dra::DraDrivers;
use crate::runtime::csi::CsiDrivers;
use anyhow::{Context, Result};
use std::collections::{HashMap, HashSet};
use std::os::unix::fs::FileTypeExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tonic::transport::{Channel, Endpoint, Uri};
use tracing::{info, warn};

pub mod v1 {
    tonic::include_proto!("pluginregistration");
}

use v1::registration_client::RegistrationClient;
use v1::{InfoRequest, RegistrationStatus};

const CSI_PLUGIN_TYPE: &str = "CSIPlugin";
const DEVICE_PLUGIN_TYPE: &str = "DevicePlugin";
const DRA_PLUGIN_TYPE: &str = "DRAPlugin";

/// Which backend a registered socket belongs to — tracked alongside its
/// name so a socket's disappearance can be routed to the right
/// `deregister()`.
enum PluginKind {
    Csi,
    Device,
    Dra,
}

/// Dial a plugin's registration socket — same connector shape as
/// `runtime/cri.rs::connect_uds`/`runtime/csi.rs::connect_uds`, a third
/// independent copy for the same reason csi.rs's is: different proto
/// package, no reason to couple the modules together over a ~15-line helper.
async fn connect_uds(path: &Path) -> Result<Channel> {
    let path = path.to_path_buf();
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
        .context("connecting to plugin registration socket")
}

/// Every Unix socket file directly inside `dir` — pure enough (given a real
/// directory to scan) to unit test without a live registrar process, by
/// creating throwaway `UnixListener` sockets in a scratch directory.
/// Non-socket files and subdirectories are ignored, matching real
/// kubelet's plugin watcher, which only reacts to socket files.
fn scan_registry_dir(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else { return Vec::new() };
    entries
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().map(|t| t.is_socket()).unwrap_or(false))
        .map(|e| e.path())
        .collect()
}

/// Dial `socket_path`, exchange `GetInfo`/`NotifyRegistrationStatus`, and
/// register the driver with `csi` if it's a CSI plugin with a usable name
/// and endpoint. Returns the driver name on success (so the caller can
/// track it for deregistration later), `None` for a plugin type nodelet
/// doesn't support (already told no via `NotifyRegistrationStatus`, not
/// silently dropped).
async fn register_one(
    csi: &Arc<CsiDrivers>,
    devices: &Arc<DevicePlugins>,
    dra: &Arc<DraDrivers>,
    socket_path: &Path,
) -> Result<Option<(PluginKind, String)>> {
    let channel = connect_uds(socket_path).await?;
    let mut client = RegistrationClient::new(channel);
    let info = client.get_info(InfoRequest {}).await.context("GetInfo")?.into_inner();

    let kind = match info.r#type.as_str() {
        CSI_PLUGIN_TYPE => PluginKind::Csi,
        DEVICE_PLUGIN_TYPE => PluginKind::Device,
        DRA_PLUGIN_TYPE => PluginKind::Dra,
        other => {
            let _ = client
                .notify_registration_status(RegistrationStatus {
                    plugin_registered: false,
                    error: format!("nodelet only supports {CSI_PLUGIN_TYPE}/{DEVICE_PLUGIN_TYPE}/{DRA_PLUGIN_TYPE} registrations, got '{other}'"),
                })
                .await;
            info!(plugin = %info.name, plugin_type = %other, "plugin registry: rejecting unsupported plugin type");
            return Ok(None);
        }
    };
    if info.name.is_empty() || info.endpoint.is_empty() {
        let _ = client
            .notify_registration_status(RegistrationStatus {
                plugin_registered: false,
                error: "PluginInfo.name and .endpoint are both required".to_string(),
            })
            .await;
        anyhow::bail!("PluginInfo missing name or endpoint");
    }

    match kind {
        PluginKind::Csi => csi.register(info.name.clone(), info.endpoint.clone()),
        PluginKind::Device => devices.register(info.name.clone(), info.endpoint.clone()),
        PluginKind::Dra => dra.register(info.name.clone(), info.endpoint.clone()),
    }
    client
        .notify_registration_status(RegistrationStatus { plugin_registered: true, error: String::new() })
        .await
        .context("NotifyRegistrationStatus")?;
    info!(name = %info.name, endpoint = %info.endpoint, plugin_type = %info.r#type, "plugin registry: plugin registered");
    Ok(Some((kind, info.name)))
}

/// Watch `registry_path` forever, registering/deregistering CSI drivers
/// (with `csi`) and device plugins (with `devices`) as their sockets
/// appear/disappear. Never returns under normal operation. If
/// `registry_path` can't even be created, logs once and returns — dynamic
/// discovery is simply unavailable for this run (static
/// `NODELET_CSI_DRIVERS` config still works either way; there's no static
/// equivalent for device plugins, so a failure here means device plugins
/// don't work at all for this run).
pub async fn run(csi: Arc<CsiDrivers>, devices: Arc<DevicePlugins>, dra: Arc<DraDrivers>, registry_path: String, sync_interval: Duration) {
    let dir = PathBuf::from(&registry_path);
    if let Err(e) = std::fs::create_dir_all(&dir) {
        warn!(path = %dir.display(), error = ?e, "plugin registry: couldn't create the registry directory; dynamic plugin discovery disabled for this run");
        return;
    }
    info!(path = %dir.display(), "plugin registry: watching for CSI driver / device plugin registrations");

    // socket path -> (which backend, plugin name), so a socket's
    // disappearance can be routed to the right deregister() without
    // re-dialing it.
    let mut known: HashMap<PathBuf, (PluginKind, String)> = HashMap::new();

    loop {
        let present: HashSet<PathBuf> = scan_registry_dir(&dir).into_iter().collect();

        let gone: Vec<PathBuf> = known.keys().filter(|p| !present.contains(*p)).cloned().collect();
        for path in gone {
            if let Some((kind, name)) = known.remove(&path) {
                info!(name, path = %path.display(), "plugin registry: socket disappeared; deregistering");
                match kind {
                    PluginKind::Csi => csi.deregister(&name),
                    PluginKind::Device => devices.deregister(&name),
                    PluginKind::Dra => dra.deregister(&name),
                }
            }
        }

        for path in &present {
            if known.contains_key(path) {
                continue;
            }
            match register_one(&csi, &devices, &dra, path).await {
                Ok(Some(entry)) => {
                    known.insert(path.clone(), entry);
                }
                Ok(None) => {} // logged inside register_one — not a supported plugin type
                Err(e) => warn!(path = %path.display(), error = ?e, "plugin registry: registration attempt failed"),
            }
        }

        tokio::time::sleep(sync_interval).await;
    }
}

#[cfg(test)]
#[path = "plugin_registry_tests/scan_registry_dir.rs"]
mod tests_scan_registry_dir;
