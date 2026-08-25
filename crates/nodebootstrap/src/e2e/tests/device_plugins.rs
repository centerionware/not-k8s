use super::context::E2eContext;
use super::skip_test;
use anyhow::{Context, Result};
use k8s_openapi::api::core::v1::{Node, Pod};
use kube::api::{Api, AttachParams, DeleteParams, ListParams, PostParams};
use serde_json::{json, Value};
use std::collections::HashSet;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::Command;
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::net::UnixListener;
use tokio::io::AsyncReadExt;
use tokio::sync::{mpsc, oneshot};
use tokio_stream::{wrappers::ReceiverStream, wrappers::UnixListenerStream, Stream};
use tonic::{Request, Response, Status};

mod pluginregistration {
    tonic::include_proto!("pluginregistration");
}

mod deviceplugin {
    tonic::include_proto!("v1beta1");
}

use deviceplugin::device_plugin_server::{DevicePlugin, DevicePluginServer};
use deviceplugin::{
    AllocateRequest, AllocateResponse, ContainerAllocateResponse,
    ContainerPreferredAllocationResponse, Device, DevicePluginOptions, Empty,
    ListAndWatchResponse, PreStartContainerRequest, PreStartContainerResponse,
    PreferredAllocationRequest, PreferredAllocationResponse,
};
use pluginregistration::registration_server::{Registration, RegistrationServer};
use pluginregistration::{
    InfoRequest, PluginInfo, RegistrationStatus, RegistrationStatusResponse,
};

const FAKE_RESOURCE: &str = "fake.example.com/testdevice";
const DEVICE_IDS: [&str; 4] = ["fake-0", "fake-1", "fake-2", "fake-3"];

fn needs_cri() -> Result<()> {
    anyhow::ensure!(
        crate::config::Config::from_env()?.nodelet_runtime() == "cri",
        "device-resource status checks require the CRI runtime",
    );
    Ok(())
}

fn plugin_registry_path() -> Result<PathBuf> {
    let path = std::env::var("NODELET_PLUGIN_REGISTRY_PATH")
        .unwrap_or_else(|_| "/var/lib/nodelet/plugins_registry".to_owned());
    if Path::new(&path).is_dir() {
        Ok(path.into())
    } else {
        Err(skip_test(format!(
            "plugin registry directory {path} is not present on this deployment"
        )))
    }
}

fn root_command(program: &str, args: &[&str]) -> std::process::Command {
    let root = std::process::Command::new("id")
        .arg("-u")
        .output()
        .is_ok_and(|output| String::from_utf8_lossy(&output.stdout).trim() == "0");
    let mut command = if root {
        let mut command = std::process::Command::new(program);
        command.args(args);
        command
    } else {
        let mut command = std::process::Command::new("sudo");
        command.arg("-n").arg(program).args(args);
        command
    };
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

#[derive(Default)]
struct PluginState {
    unhealthy: HashSet<String>,
    fail_preferred: bool,
    prestart: Vec<Vec<String>>,
}

#[derive(Clone)]
struct RegistrationService {
    endpoint: String,
}

#[tonic::async_trait]
impl Registration for RegistrationService {
    async fn get_info(&self, _request: Request<InfoRequest>) -> Result<Response<PluginInfo>, Status> {
        Ok(Response::new(PluginInfo {
            r#type: "DevicePlugin".to_owned(),
            name: FAKE_RESOURCE.to_owned(),
            endpoint: self.endpoint.clone(),
            supported_versions: vec!["v1beta1".to_owned()],
        }))
    }

    async fn notify_registration_status(
        &self,
        _request: Request<RegistrationStatus>,
    ) -> Result<Response<RegistrationStatusResponse>, Status> {
        Ok(Response::new(RegistrationStatusResponse {}))
    }
}

#[derive(Clone)]
struct DevicePluginService {
    state: Arc<Mutex<PluginState>>,
}

#[tonic::async_trait]
impl DevicePlugin for DevicePluginService {
    async fn get_device_plugin_options(
        &self,
        _request: Request<Empty>,
    ) -> Result<Response<DevicePluginOptions>, Status> {
        Ok(Response::new(DevicePluginOptions {
            pre_start_required: true,
            get_preferred_allocation_available: true,
        }))
    }

    type ListAndWatchStream =
        Pin<Box<dyn Stream<Item = Result<ListAndWatchResponse, Status>> + Send + 'static>>;

    async fn list_and_watch(
        &self,
        _request: Request<Empty>,
    ) -> Result<Response<Self::ListAndWatchStream>, Status> {
        let state = Arc::clone(&self.state);
        let (sender, receiver) = mpsc::channel(4);
        tokio::spawn(async move {
            let mut previous = HashSet::new();
            loop {
                let current = state
                    .lock()
                    .map(|state| state.unhealthy.clone())
                    .unwrap_or_default();
                if current != previous {
                    let devices = DEVICE_IDS
                        .iter()
                        .map(|id| Device {
                            id: (*id).to_owned(),
                            health: if current.contains(*id) {
                                "Unhealthy".to_owned()
                            } else {
                                "Healthy".to_owned()
                            },
                            topology: None,
                        })
                        .collect();
                    if sender
                        .send(Ok(ListAndWatchResponse { devices }))
                        .await
                        .is_err()
                    {
                        break;
                    }
                    previous = current;
                }
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        });
        Ok(Response::new(Box::pin(ReceiverStream::new(receiver))))
    }

    async fn get_preferred_allocation(
        &self,
        request: Request<PreferredAllocationRequest>,
    ) -> Result<Response<PreferredAllocationResponse>, Status> {
        if self
            .state
            .lock()
            .map(|state| state.fail_preferred)
            .unwrap_or(false)
        {
            return Err(Status::internal(
                "deliberate e2e preferred-allocation failure",
            ));
        }
        let container_responses = request
            .into_inner()
            .container_requests
            .into_iter()
            .map(|container| {
                let mut selected = container.must_include_device_i_ds;
                for device in container.available_device_i_ds.into_iter().rev() {
                    if selected.len() >= container.allocation_size as usize {
                        break;
                    }
                    if !selected.contains(&device) {
                        selected.push(device);
                    }
                }
                ContainerPreferredAllocationResponse {
                    device_i_ds: selected
                        .into_iter()
                        .take(container.allocation_size as usize)
                        .collect(),
                }
            })
            .collect();
        Ok(Response::new(PreferredAllocationResponse {
            container_responses,
        }))
    }

    async fn allocate(
        &self,
        request: Request<AllocateRequest>,
    ) -> Result<Response<AllocateResponse>, Status> {
        let container_responses = request
            .into_inner()
            .container_requests
            .into_iter()
            .map(|container| {
                let mut response = ContainerAllocateResponse::default();
                let mut ids = container.devices_ids;
                ids.sort();
                response
                    .envs
                    .insert("FAKE_DEVICE_IDS".to_owned(), ids.join(","));
                response
            })
            .collect();
        Ok(Response::new(AllocateResponse {
            container_responses,
        }))
    }

    async fn pre_start_container(
        &self,
        request: Request<PreStartContainerRequest>,
    ) -> Result<Response<PreStartContainerResponse>, Status> {
        if let Ok(mut state) = self.state.lock() {
            let mut ids = request.into_inner().devices_ids;
            ids.sort();
            state.prestart.push(ids);
        }
        Ok(Response::new(PreStartContainerResponse {}))
    }
}

struct FakeDevicePlugin {
    registry: PathBuf,
    registry_mode: Option<u32>,
    socket: PathBuf,
    state: Arc<Mutex<PluginState>>,
    shutdown: Option<oneshot::Sender<()>>,
    task: Option<tokio::task::JoinHandle<Result<(), tonic::transport::Error>>>,
}

impl FakeDevicePlugin {
    async fn start(context: &E2eContext) -> Result<Self> {
        needs_cri()?;
        let registry = plugin_registry_path()?;
        let original_mode = fs::metadata(&registry)
            .context("reading plugin registry permissions")?
            .permissions()
            .mode()
            & 0o777;
        let socket = registry.join(format!(
            "nodebootstrap-device-{}.sock",
            std::process::id()
        ));
        let _ = fs::remove_file(&socket);
        let (listener, registry_mode) = match UnixListener::bind(&socket) {
            Ok(listener) => (listener, None),
            Err(first_error) => {
                let mode = "0777";
                if let Err(chmod_error) =
                    run_root("chmod", &[mode, registry.to_str().unwrap_or_default()])
                {
                    return Err(skip_test(format!(
                        "cannot bind fake device-plugin socket {} ({first_error}) and cannot grant temporary registry access ({chmod_error})",
                        socket.display()
                    )));
                }
                match UnixListener::bind(&socket) {
                    Ok(listener) => (listener, Some(original_mode)),
                    Err(error) => {
                        let restore = format!("{original_mode:04o}");
                        let _ = run_root(
                            "chmod",
                            &[&restore, registry.to_str().unwrap_or_default()],
                        );
                        return Err(anyhow::anyhow!(
                            "binding fake device-plugin socket {} after chmod: {error}",
                            socket.display()
                        ));
                    }
                }
            }
        };
        let endpoint = socket.to_string_lossy().into_owned();
        let state = Arc::new(Mutex::new(PluginState::default()));
        let registration = RegistrationServer::new(RegistrationService { endpoint });
        let device = DevicePluginServer::new(DevicePluginService {
            state: Arc::clone(&state),
        });
        let incoming = UnixListenerStream::new(listener);
        let (shutdown, signal) = oneshot::channel();
        let task = tokio::spawn(async move {
            tonic::transport::Server::builder()
                .add_service(registration)
                .add_service(device)
                .serve_with_incoming_shutdown(incoming, async {
                    let _ = signal.await;
                })
                .await
        });
        let plugin = Self {
            registry,
            registry_mode,
            socket,
            state,
            shutdown: Some(shutdown),
            task: Some(task),
        };
        let nodes: Api<Node> = Api::all(context.client.clone());
        context
            .wait_until(
                "fake device capacity to reach the Node status",
                Duration::from_secs(120),
                || {
                    let nodes = nodes.clone();
                    async move {
                        let Some(node) = nodes
                            .list(&ListParams::default())
                            .await?
                            .items
                            .into_iter()
                            .next()
                        else {
                            return Ok(false);
                        };
                        let status = serde_json::to_value(node.status)?;
                        Ok(status
                            .pointer("/capacity/fake.example.com~1testdevice")
                            .and_then(Value::as_str)
                            == Some("4"))
                    }
                },
            )
            .await?;
        Ok(plugin)
    }

    async fn stop(&mut self, context: &E2eContext) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
        let _ = fs::remove_file(&self.socket);
        self.restore_registry_mode();
        let nodes: Api<Node> = Api::all(context.client.clone());
        let _ = context
            .wait_until(
                "fake device capacity to disappear",
                Duration::from_secs(120),
                || {
                    let nodes = nodes.clone();
                    async move {
                        let Some(node) = nodes
                            .list(&ListParams::default())
                            .await?
                            .items
                            .into_iter()
                            .next()
                        else {
                            return Ok(true);
                        };
                        let status = serde_json::to_value(node.status)?;
                        Ok(status
                            .pointer("/capacity/fake.example.com~1testdevice")
                            .is_none())
                    }
                },
            )
            .await;
    }

    fn set_unhealthy(&self, device: &str) {
        if let Ok(mut state) = self.state.lock() {
            state.unhealthy = device
                .split(',')
                .filter(|id| !id.is_empty())
                .map(str::to_owned)
                .collect();
        }
    }

    fn fail_preferred(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.fail_preferred = true;
        }
    }

    fn prestart_contains(&self, expected: &str) -> bool {
        self.state
            .lock()
            .map(|state| state.prestart.iter().any(|ids| ids.join(",") == expected))
            .unwrap_or(false)
    }

    fn restore_registry_mode(&mut self) {
        if let Some(mode) = self.registry_mode.take() {
            let mode = format!("{mode:04o}");
            let _ = run_root(
                "chmod",
                &[&mode, self.registry.to_str().unwrap_or_default()],
            );
        }
    }
}

impl Drop for FakeDevicePlugin {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        if let Some(task) = self.task.take() {
            task.abort();
        }
        let _ = fs::remove_file(&self.socket);
        self.restore_registry_mode();
    }
}

async fn create_device_pod(
    context: &E2eContext,
    name: &str,
    count: &str,
) -> Result<Api<Pod>> {
    let pods: Api<Pod> = Api::namespaced(context.client.clone(), &context.namespace);
    let mut pod = json!({
        "apiVersion": "v1",
        "kind": "Pod",
        "metadata": {"name": name},
        "spec": {
            "containers": [{
                "name": "app",
                "image": "busybox:latest",
                "command": ["sleep", "3600"],
                "resources": {"limits": {}}
            }]
        }
    });
    pod["spec"]["containers"][0]["resources"]["limits"][FAKE_RESOURCE] = json!(count);
    let pod: Pod = serde_json::from_value(pod)?;
    pods.create(&PostParams::default(), &pod).await?;
    context
        .wait_until("device test Pod to reach Running", Duration::from_secs(90), || {
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
    Ok(pods)
}

async fn exec_output(context: &E2eContext, pod: &str, command: &[&str]) -> Result<String> {
    let pods: Api<Pod> = Api::namespaced(context.client.clone(), &context.namespace);
    let params = AttachParams::default()
        .container("app")
        .stdout(true)
        .stderr(false);
    let mut process = pods.exec(pod, command.iter().copied(), &params).await?;
    let mut stdout = Vec::new();
    if let Some(mut stream) = process.stdout() {
        stream.read_to_end(&mut stdout).await?;
    }
    process.join().await?;
    Ok(String::from_utf8_lossy(&stdout).into_owned())
}

fn allocated_status(pod: &Pod) -> Option<Value> {
    serde_json::to_value(pod)
        .ok()
        .and_then(|pod| pod.pointer("/status/containerStatuses/0/allocatedResourcesStatus").cloned())
}

async fn wait_for_allocated_status(
    context: &E2eContext,
    pods: &Api<Pod>,
    name: &str,
    text: &str,
) -> Result<()> {
    context
        .wait_until("allocated device status", Duration::from_secs(90), || {
            let pods = pods.clone();
            async move {
                Ok(allocated_status(&pods.get(name).await?)
                    .is_some_and(|status| status.to_string().contains(text)))
            }
        })
        .await
}

pub(super) async fn plugin_registry_watches_for_device_plugins_too(
    _context: &E2eContext,
) -> Result<()> {
    needs_cri().map_err(|error| skip_test(error.to_string()))?;
    plugin_registry_path().map(|_| ())
}

pub(super) async fn allocated_resources_status_absent_without_device_resources(
    context: &E2eContext,
) -> Result<()> {
    needs_cri().map_err(|error| skip_test(error.to_string()))?;
    let name = "no-device-resources";
    let pods: Api<Pod> = Api::namespaced(context.client.clone(), &context.namespace);
    let pod: Pod = serde_json::from_value(json!({
        "apiVersion": "v1",
        "kind": "Pod",
        "metadata": {"name": name},
        "spec": {
            "containers": [{"name": "app", "image": "busybox:latest", "command": ["sleep", "3600"]}]
        }
    }))?;
    pods.create(&PostParams::default(), &pod).await?;
    context
        .wait_until("plain Pod to reach Running without device resources", Duration::from_secs(90), || {
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
    let status = serde_json::to_value(pods.get(name).await?)?;
    let allocated = status.pointer("/status/containerStatuses/0/allocatedResourcesStatus");
    anyhow::ensure!(
        !allocated.is_some_and(|value| {
            !value.is_null() && !value.as_object().is_some_and(|object| object.is_empty())
        }),
        "a Pod without device-plugin resources unexpectedly reported allocatedResourcesStatus: {allocated:?}"
    );
    Ok(())
}

pub(super) async fn device_plugin_advertises_capacity_and_allocates_into_a_container(
    context: &E2eContext,
) -> Result<()> {
    let mut plugin = FakeDevicePlugin::start(context).await?;
    let result = async {
        let pods = create_device_pod(context, "device-plugin-alloc-check", "1").await?;
        wait_for_allocated_status(context, &pods, "device-plugin-alloc-check", "Healthy").await?;
        let env = exec_output(
            context,
            "device-plugin-alloc-check",
            &["sh", "-c", "echo $FAKE_DEVICE_IDS"],
        )
        .await?;
        anyhow::ensure!(
            env.contains("fake-"),
            "Allocate did not inject FAKE_DEVICE_IDS into the container: {env:?}"
        );
        Ok(())
    }
    .await;
    plugin.stop(context).await;
    result
}

pub(super) async fn device_plugin_health_transition_updates_allocated_resources_status(
    context: &E2eContext,
) -> Result<()> {
    let mut plugin = FakeDevicePlugin::start(context).await?;
    let result = async {
        let pod_name = "device-plugin-health-check";
        let pods = create_device_pod(context, pod_name, "1").await?;
        let device =
            exec_output(context, pod_name, &["sh", "-c", "echo $FAKE_DEVICE_IDS"]).await?;
        anyhow::ensure!(!device.trim().is_empty(), "device allocation env was empty");
        plugin.set_unhealthy(device.trim());
        wait_for_allocated_status(context, &pods, pod_name, "Unhealthy").await?;
        anyhow::ensure!(
            pods
                .get(pod_name)
                .await?
                .status
                .and_then(|status| status.phase)
                .as_deref()
                == Some("Running"),
            "device health update restarted the container"
        );
        Ok(())
    }
    .await;
    plugin.stop(context).await;
    result
}

pub(super) async fn device_plugin_preferred_allocation_and_prestart(
    context: &E2eContext,
) -> Result<()> {
    let mut plugin = FakeDevicePlugin::start(context).await?;
    let result = async {
        let name = "device-plugin-preferred-ok";
        let pods = create_device_pod(context, name, "2").await?;
        let ids = exec_output(context, name, &["sh", "-c", "echo $FAKE_DEVICE_IDS"]).await?;
        anyhow::ensure!(
            ids.trim() == "fake-2,fake-3",
            "GetPreferredAllocation response was not used: {:?}",
            ids.trim()
        );
        anyhow::ensure!(
            plugin.prestart_contains("fake-2,fake-3"),
            "PreStartContainer was not called for the preferred devices"
        );
        pods.delete(name, &DeleteParams::default()).await?;
        plugin.fail_preferred();
        let fallback = "device-plugin-preferred-fallback";
        let _ = create_device_pod(context, fallback, "1").await?;
        anyhow::ensure!(
            Command::new("journalctl")
                .args(["-u", "nodelet", "--no-pager"])
                .output()
                .map(|output| {
                    String::from_utf8_lossy(&output.stdout)
                        .contains("GetPreferredAllocation failed; falling back")
                })
                .unwrap_or(false),
            "nodelet did not log the preferred-allocation fallback"
        );
        Ok(())
    }
    .await;
    plugin.stop(context).await;
    result
}
