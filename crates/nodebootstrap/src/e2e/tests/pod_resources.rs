use super::context::E2eContext;
use super::grpc::podresources;
use super::skip_test;
use anyhow::{Context, Result};
use std::fs;
use std::os::unix::fs::FileTypeExt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use k8s_openapi::api::core::v1::Node;
use k8s_openapi::api::core::v1::Pod;
use k8s_openapi::api::storage::v1::CSINode;
use kube::api::{Api, ListParams, PostParams};
use serde_json::json;
use std::time::Duration;
use tonic::transport::{Channel, Endpoint, Uri};
use tokio::net::UnixStream;

type PodResourcesClient = podresources::pod_resources_lister_client::PodResourcesListerClient<Channel>;

async fn connect_pod_resources(socket: &Path) -> Result<PodResourcesClient> {
    let socket = socket.to_path_buf();
    let channel = Endpoint::try_from("http://localhost")
        .context("invalid PodResources endpoint")?
        .connect_with_connector(tower::service_fn(move |_: Uri| {
            let socket = socket.clone();
            async move {
                let stream = UnixStream::connect(socket).await?;
                Ok::<_, std::io::Error>(hyper_util::rt::TokioIo::new(stream))
            }
        }))
        .await
        .context("connecting to the PodResources Unix socket")?;
    Ok(PodResourcesClient::new(channel))
}

fn root_command(program: &str, args: &[&str]) -> std::process::Command {
    let is_root = std::process::Command::new("id")
        .arg("-u")
        .output()
        .is_ok_and(|output| String::from_utf8_lossy(&output.stdout).trim() == "0");
    let mut command = if is_root {
        std::process::Command::new(program)
    } else {
        let mut command = std::process::Command::new("sudo");
        command.arg("-n").arg(program);
        command
    };
    command.args(args);
    command
}

fn run_root(program: &str, args: &[&str]) -> Result<()> {
    let output = root_command(program, args).output()?;
    anyhow::ensure!(
        output.status.success(),
        "{program} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(())
}

struct SocketModeGuard {
    path: PathBuf,
    original_mode: u32,
}

impl SocketModeGuard {
    fn grant(path: &Path) -> Result<Self> {
        let original_mode = fs::metadata(path)?.permissions().mode() & 0o777;
        let path_string = path.to_string_lossy().into_owned();
        run_root("chmod", &["0666", &path_string])?;
        Ok(Self {
            path: path.to_path_buf(),
            original_mode,
        })
    }
}

impl Drop for SocketModeGuard {
    fn drop(&mut self) {
        let mode = format!("{:04o}", self.original_mode);
        let path = self.path.to_string_lossy().into_owned();
        let _ = run_root("chmod", &[&mode, &path]);
    }
}

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
    let socket_path = Path::new(&socket);
    if !socket_path.exists() {
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
        let mut _socket_access = None;
        let mut client = match connect_pod_resources(socket_path).await {
            Ok(client) => client,
            Err(first_error) => {
                _socket_access = Some(SocketModeGuard::grant(socket_path).map_err(|access_error| {
                    anyhow::anyhow!(
                        "connecting to PodResources failed ({first_error}); temporary socket access also failed: {access_error}"
                    )
                })?);
                connect_pod_resources(socket_path)
                    .await
                    .context("connecting to PodResources after granting temporary socket access")?
            }
        };
        let list = client
            .list(podresources::ListPodResourcesRequest {})
            .await
            .context("PodResources List RPC failed")?
            .into_inner();
        anyhow::ensure!(
            list.pod_resources
                .iter()
                .any(|pod| pod.name == name && pod.namespace == context.namespace),
            "PodResources List did not include the running Pod {name}"
        );
        match client
            .get(podresources::GetPodResourcesRequest {
                pod_name: "does-not-exist".to_string(),
                pod_namespace: context.namespace.clone(),
            })
            .await
        {
            Err(status) if status.code() == tonic::Code::NotFound => {}
            Err(status) => anyhow::bail!(
                "PodResources Get for a nonexistent Pod returned {:?}: {}",
                status.code(),
                status.message()
            ),
            Ok(_) => anyhow::bail!(
                "PodResources Get for a nonexistent Pod unexpectedly succeeded"
            ),
        }
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
