use super::context::E2eContext;
use super::skip_test;
use anyhow::Result;
use std::os::unix::fs::FileTypeExt;
use std::path::Path;
use k8s_openapi::api::core::v1::Node;
use k8s_openapi::api::storage::v1::CSINode;
use kube::api::{Api, ListParams};
use std::time::Duration;

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

pub(super) async fn dynamic_csi_registration_is_visible_on_the_node(
    context: &E2eContext,
) -> Result<()> {
    let driver = std::env::var("TEST_CSI_INLINE_DRIVER")
        .ok()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            skip_test(
                "TEST_CSI_INLINE_DRIVER is not set; a dynamically registered CSI driver is required",
            )
        })?;
    let nodes: Api<Node> = Api::all(context.client.clone());
    let node_name = nodes
        .list(&ListParams::default())
        .await?
        .items
        .into_iter()
        .find_map(|node| node.metadata.name)
        .ok_or_else(|| anyhow::anyhow!("the cluster has no Node object"))?;
    let csinodes: Api<CSINode> = Api::all(context.client.clone());
    context
        .wait_until("CSI driver registration on the node", Duration::from_secs(90), || {
            let csinodes = csinodes.clone();
            let node_name = node_name.clone();
            let driver = driver.clone();
            async move {
                Ok(csinodes
                    .get(&node_name)
                    .await?
                    .spec
                    .map(|spec| spec.drivers)
                    .unwrap_or_default()
                    .into_iter()
                    .any(|registered| registered.name == driver))
            }
        })
        .await
}
