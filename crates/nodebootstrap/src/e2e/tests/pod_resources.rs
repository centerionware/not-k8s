use super::context::E2eContext;
use super::skip_test;
use anyhow::{Context, Result};
use std::os::unix::fs::FileTypeExt;
use std::path::Path;
use k8s_openapi::api::core::v1::Node;
use k8s_openapi::api::core::v1::Pod;
use k8s_openapi::api::storage::v1::CSINode;
use kube::api::{Api, ListParams, PostParams};
use serde_json::json;
use std::process::Command;
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

pub(super) async fn pod_resources_grpc_query_returns_real_data(
    context: &E2eContext,
) -> Result<()> {
    if crate::config::Config::from_env()?.nodelet_runtime() != "cri" {
        return Err(skip_test("PodResources gRPC checks require the CRI runtime"));
    }
    let socket = std::env::var("NODELET_POD_RESOURCES_SOCKET_PATH")
        .unwrap_or_else(|_| "/var/lib/nodelet/pod-resources/kubelet.sock".to_string());
    if socket.is_empty() {
        return Err(skip_test("PodResources API is disabled on this deployment"));
    }
    let grpcurl_available = std::env::var_os("PATH").is_some_and(|paths| {
        std::env::split_paths(&paths).any(|directory| directory.join("grpcurl").is_file())
    });
    if !grpcurl_available {
        return Err(skip_test(
            "grpcurl is not on PATH; the full e2e setup installs it for PodResources checks",
        ));
    }
    let proto = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../nodelet/proto/podresources.proto");
    if !proto.is_file() {
        return Err(skip_test(format!(
            "PodResources proto is not present at {}",
            proto.display()
        )));
    }
    if !Path::new(&socket).is_file() && !Path::new(&socket).exists() {
        return Err(skip_test(format!(
            "PodResources socket {socket} is not present on this deployment"
        )));
    }
    let name = "pod-resources-grpc-check";
    let pods: Api<Pod> = Api::namespaced(context.client.clone(), &context.namespace);
    let pod: Pod = serde_json::from_value(json!({
        "apiVersion": "v1",
        "kind": "Pod",
        "metadata": {"name": name},
        "spec": {"containers": [{"name": "app", "image": "busybox:latest", "command": ["sleep", "3600"]}]}
    }))?;
    pods.create(&PostParams::default(), &pod).await?;
    let proto_dir = proto
        .parent()
        .context("PodResources proto has no parent directory")?;
    let grpcurl = |args: &[&str]| -> Result<(bool, String)> {
        let output = Command::new("sudo")
            .arg("grpcurl")
            .args(["-plaintext", "-unix", "-import-path"])
            .arg(proto_dir)
            .args(["-proto"])
            .arg(&proto)
            .args(args)
            .arg(&socket)
            .output()
            .context("running sudo grpcurl against PodResources")?;
        let output_text = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        Ok((output.status.success(), output_text))
    };
    let result = async {
        context
            .wait_until("PodResources test Pod Running", Duration::from_secs(90), || {
                let pods = pods.clone();
                async move {
                    Ok(pods
                        .get(name)
                        .await?
                        .status
                        .and_then(|status| status.phase)
                        .as_deref()
                        == Some("Running"))
                }
            })
            .await?;
        let (list_ok, list_output) = grpcurl(&["v1.PodResourcesLister/List"])?;
        anyhow::ensure!(list_ok, "PodResources List failed: {list_output}");
        anyhow::ensure!(
            list_output.contains(name),
            "PodResources List did not include the running Pod {name}: {list_output}"
        );
        let (get_ok, get_output) = grpcurl(&[
            "-d",
            &format!("{{\"podName\":\"does-not-exist\",\"podNamespace\":\"{}\"}}", context.namespace),
            "v1.PodResourcesLister/Get",
        ])?;
        anyhow::ensure!(!get_ok, "PodResources Get for a nonexistent Pod unexpectedly succeeded");
        anyhow::ensure!(
            get_output.contains("NotFound"),
            "PodResources Get did not return NotFound: {get_output}"
        );
        Ok(())
    }
    .await;
    let _ = pods.delete(name, &kube::api::DeleteParams::default()).await;
    result
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
                    .drivers
                    .into_iter()
                    .any(|registered| registered.name == driver))
            }
        })
        .await
}
