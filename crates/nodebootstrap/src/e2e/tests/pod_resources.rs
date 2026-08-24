use super::context::E2eContext;
use super::skip_test;
use anyhow::Result;
use std::path::Path;
use std::os::unix::fs::FileTypeExt;

pub(super) async fn pod_resources_socket_is_created_on_a_cri_node(
    _context: &E2eContext,
) -> Result<()> {
    let path = std::env::var("NODELET_POD_RESOURCES_SOCKET_PATH")
        .unwrap_or_else(|_| "/var/lib/nodelet/pod-resources/kubelet.sock".to_string());
    if path.is_empty() {
        return Err(skip_test(
            "NODELET_POD_RESOURCES_SOCKET_PATH is empty; the PodResources API is disabled",
        ));
    }
    if !Path::new(&path).exists() {
        return Err(skip_test(format!(
            "PodResources socket {path} is not present on this deployment"
        )));
    }
    anyhow::ensure!(
        Path::new(&path).metadata()?.file_type().is_socket(),
        "PodResources path {path} exists but is not a Unix socket"
    );
    Ok(())
}

pub(super) async fn plugin_registry_directory_exists(_context: &E2eContext) -> Result<()> {
    let path = std::env::var("NODELET_PLUGIN_REGISTRY_PATH")
        .unwrap_or_else(|_| "/var/lib/nodelet/plugins_registry".to_string());
    if path.is_empty() {
        return Err(skip_test(
            "NODELET_PLUGIN_REGISTRY_PATH is explicitly empty; plugin discovery is disabled",
        ));
    }
    if !Path::new(&path).is_dir() {
        return Err(skip_test(format!(
            "plugin registry directory {path} does not exist on this deployment"
        )));
    }
    Ok(())
}
