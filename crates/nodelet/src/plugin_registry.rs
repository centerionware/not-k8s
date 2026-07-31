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
//! **Scope**: only `PluginInfo.type == "CSIPlugin"` is handled — device
//! plugins use this exact same protocol but nodelet doesn't implement the
//! DevicePlugin gRPC API itself (see docs/GAP_CLOSURE.md), so a device
//! plugin's registration attempt is explicitly rejected via
//! `NotifyRegistrationStatus{plugin_registered: false, ...}` rather than
//! silently ignored — the plugin gets a real answer either way, matching
//! what a registrar sidecar actually expects to receive.

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
async fn register_one(csi: &CsiDrivers, socket_path: &Path) -> Result<Option<String>> {
    let channel = connect_uds(socket_path).await?;
    let mut client = RegistrationClient::new(channel);
    let info = client.get_info(InfoRequest {}).await.context("GetInfo")?.into_inner();

    if info.r#type != CSI_PLUGIN_TYPE {
        let _ = client
            .notify_registration_status(RegistrationStatus {
                plugin_registered: false,
                error: format!("nodelet only supports {CSI_PLUGIN_TYPE} registrations, got '{}'", info.r#type),
            })
            .await;
        info!(plugin = %info.name, plugin_type = %info.r#type, "plugin registry: rejecting non-CSI plugin (device plugins aren't implemented)");
        return Ok(None);
    }
    if info.name.is_empty() || info.endpoint.is_empty() {
        let _ = client
            .notify_registration_status(RegistrationStatus {
                plugin_registered: false,
                error: "PluginInfo.name and .endpoint are both required".to_string(),
            })
            .await;
        anyhow::bail!("PluginInfo missing name or endpoint");
    }

    csi.register(info.name.clone(), info.endpoint.clone());
    client
        .notify_registration_status(RegistrationStatus { plugin_registered: true, error: String::new() })
        .await
        .context("NotifyRegistrationStatus")?;
    info!(driver = %info.name, endpoint = %info.endpoint, "plugin registry: CSI driver registered");
    Ok(Some(info.name))
}

/// Watch `registry_path` forever, registering/deregistering CSI drivers
/// with `csi` as their sockets appear/disappear. Never returns under
/// normal operation. If `registry_path` can't even be created, logs once
/// and returns — dynamic discovery is simply unavailable for this run
/// (static `NODELET_CSI_DRIVERS` config still works either way).
pub async fn run(csi: Arc<CsiDrivers>, registry_path: String, sync_interval: Duration) {
    let dir = PathBuf::from(&registry_path);
    if let Err(e) = std::fs::create_dir_all(&dir) {
        warn!(path = %dir.display(), error = ?e, "plugin registry: couldn't create the registry directory; dynamic CSI driver discovery disabled for this run");
        return;
    }
    info!(path = %dir.display(), "plugin registry: watching for CSI driver registrations");

    // socket path -> driver name, so a socket's disappearance can
    // deregister the right driver without re-dialing it.
    let mut known: HashMap<PathBuf, String> = HashMap::new();

    loop {
        let present: HashSet<PathBuf> = scan_registry_dir(&dir).into_iter().collect();

        let gone: Vec<PathBuf> = known.keys().filter(|p| !present.contains(*p)).cloned().collect();
        for path in gone {
            if let Some(driver) = known.remove(&path) {
                info!(driver, path = %path.display(), "plugin registry: socket disappeared; deregistering");
                csi.deregister(&driver);
            }
        }

        for path in &present {
            if known.contains_key(path) {
                continue;
            }
            match register_one(&csi, path).await {
                Ok(Some(driver)) => {
                    known.insert(path.clone(), driver);
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
