//! Real container runtime via the containerd/CRI gRPC API (`runtime.v1`).
//!
//! Implements the pod lifecycle against any CRI runtime (containerd, CRI-O):
//! RunPodSandbox → PullImage → CreateContainer → StartContainer, and teardown
//! via StopPodSandbox/RemovePodSandbox. Pods/containers are tagged with
//! `nodelet.dev/*` labels so every operation is idempotent (we look up existing
//! sandboxes/containers by label instead of tracking state ourselves).
//!
//! Status is **event-driven**: a background task subscribes to the CRI
//! `GetContainerEvents` stream and pushes changed pod keys onto a channel — no
//! PLEG-style per-second relisting.

#![allow(clippy::needless_question_mark)]

use super::{ContainerRuntimeStatus, Phase, PodRuntime, RuntimeStatus};
use anyhow::{Context, Result};
use async_trait::async_trait;
use k8s_openapi::api::core::v1::Pod;
use k8s_openapi::jiff::Timestamp;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use tonic::transport::{Channel, Endpoint, Uri};
use tracing::{debug, info, warn};

/// Generated CRI v1 types and gRPC clients (from proto/cri.proto).
pub mod v1 {
    tonic::include_proto!("runtime.v1");
}

/// Generated containerd top-level events API (from proto/containerd_events.proto).
pub mod events {
    tonic::include_proto!("containerd.services.events.v1");
}

use events::events_client::EventsClient;
use events::{SubscribeRequest, TaskEventContainerId};
use prost::Message as _;

use v1::image_service_client::ImageServiceClient;
use v1::runtime_service_client::RuntimeServiceClient;
use v1::{
    ContainerConfig, ContainerFilter, ContainerMetadata, ContainerState, CreateContainerRequest,
    GetEventsRequest, ImageSpec, KeyValue, LinuxPodSandboxConfig, LinuxSandboxSecurityContext,
    ListContainersRequest, ListPodSandboxRequest, NamespaceMode, NamespaceOption, PodSandboxConfig,
    PodSandboxFilter, PodSandboxMetadata, PodSandboxStatusRequest, PullImageRequest,
    RemoveContainerRequest, RemovePodSandboxRequest, RunPodSandboxRequest, StartContainerRequest,
    StopPodSandboxRequest,
};

const POD_UID_LABEL: &str = "nodelet.dev/pod-uid";
const POD_NAME_LABEL: &str = "nodelet.dev/pod-name";
const POD_NS_LABEL: &str = "nodelet.dev/pod-namespace";
const CTR_NAME_LABEL: &str = "nodelet.dev/container-name";

pub struct CriRuntime {
    rt: RuntimeServiceClient<Channel>,
    img: ImageServiceClient<Channel>,
    rx: Mutex<Option<UnboundedReceiver<String>>>,
    // sandbox_id -> the owning Pod's restartPolicy ("Always"/"OnFailure"/"Never"),
    // recorded whenever ensure_pod() runs. build_status() needs this to decide
    // whether an all-exited container set means the pod is genuinely done
    // (Never/OnFailure-with-zero-exit) or just mid-restart (Always — see the
    // module-level restart-on-exit comment on ensure_container). The
    // event-driven status() path has no Pod object to read it from directly
    // (only namespace+name), hence the side table instead of a parameter.
    restart_policies: Mutex<HashMap<String, String>>,
}

/// Identity extracted from a Pod object.
struct PodId {
    namespace: String,
    name: String,
    uid: String,
    host_network: bool,
}

fn pod_id(pod: &Pod) -> PodId {
    let namespace = pod.metadata.namespace.clone().unwrap_or_else(|| "default".to_string());
    let name = pod.metadata.name.clone().unwrap_or_default();
    let uid = pod
        .metadata
        .uid
        .clone()
        .unwrap_or_else(|| format!("{namespace}_{name}"));
    let host_network = pod.spec.as_ref().and_then(|s| s.host_network).unwrap_or(false);
    PodId { namespace, name, uid, host_network }
}

/// Dial a unix-domain CRI socket (e.g. `unix:///run/containerd/containerd.sock`).
async fn connect_uds(endpoint: &str) -> Result<Channel> {
    let path = endpoint
        .strip_prefix("unix://")
        .unwrap_or(endpoint)
        .to_string();
    // The URI is a placeholder; the custom connector ignores it and dials the socket.
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
        .context("connecting to CRI unix socket")?;
    Ok(channel)
}

impl CriRuntime {
    pub async fn connect(endpoint: &str) -> Result<Self> {
        let channel = connect_uds(endpoint).await?;
        let rt = RuntimeServiceClient::new(channel.clone());
        let img = ImageServiceClient::new(channel.clone());

        // Spawn the event subscriber (event-driven status, no polling).
        let (tx, rx) = unbounded_channel();
        tokio::spawn(event_loop(channel, tx));

        Ok(Self { rt, img, rx: Mutex::new(Some(rx)), restart_policies: Mutex::new(HashMap::new()) })
    }

    /// Look up our sandbox for a pod by namespace+name. These labels are always
    /// set (from real values), so this is the stable key — unlike `pod.uid`,
    /// which the agent does not have at status/teardown time.
    async fn find_sandbox(&self, namespace: &str, name: &str) -> Result<Option<String>> {
        let mut rt = self.rt.clone();
        let filter = PodSandboxFilter {
            label_selector: HashMap::from([
                (POD_NS_LABEL.to_string(), namespace.to_string()),
                (POD_NAME_LABEL.to_string(), name.to_string()),
            ]),
            ..Default::default()
        };
        let resp = rt
            .list_pod_sandbox(ListPodSandboxRequest { filter: Some(filter) })
            .await?
            .into_inner();
        Ok(resp.items.into_iter().next().map(|s| s.id))
    }

    async fn list_pod_containers(&self, sandbox_id: &str) -> Result<Vec<v1::Container>> {
        let mut rt = self.rt.clone();
        let filter = ContainerFilter {
            pod_sandbox_id: sandbox_id.to_string(),
            ..Default::default()
        };
        let resp = rt
            .list_containers(ListContainersRequest { filter: Some(filter) })
            .await?
            .into_inner();
        Ok(resp.containers)
    }

    async fn run_sandbox(&self, id: &PodId) -> Result<String> {
        let mut rt = self.rt.clone();
        let config = sandbox_config(id);
        let resp = rt
            .run_pod_sandbox(RunPodSandboxRequest { config: Some(config), runtime_handler: String::new() })
            .await?
            .into_inner();
        Ok(resp.pod_sandbox_id)
    }

    // Restart-on-exit: without this, a container that crashes (any reason —
    // app bug, a bad Corefile, transient resource pressure) sits exited
    // forever, `already` matches by name alone regardless of state, and
    // ensure_container becomes a permanent no-op for it. build_status() then
    // sees "all containers exited" and reports the *Pod* as Succeeded — a
    // terminal phase Kubernetes' ReplicaSet controller treats as permanently
    // inactive (isPodActive excludes Succeeded/Failed), so it creates a
    // replacement. Forever, once per crash. Confirmed for real: this is
    // exactly what was driving unbounded coredns pod creation — coredns's
    // container was exiting seconds after starting, nodelet never restarted
    // it, and every single exit silently manufactured a brand new pod
    // instead of the crash-looping restart-in-place a real kubelet gives a
    // restartPolicy: Always pod (the default, and what every Deployment
    // uses). "Never" is left alone — matches the one-shot Job-style pods
    // that policy is for.
    async fn ensure_container(
        &self,
        sandbox_id: &str,
        id: &PodId,
        container: &k8s_openapi::api::core::v1::Container,
        restart_policy: &str,
    ) -> Result<()> {
        let running_v = ContainerState::ContainerRunning as i32;
        let existing = self.list_pod_containers(sandbox_id).await?;
        let existing_ctr = existing
            .iter()
            .find(|c| c.labels.get(CTR_NAME_LABEL).map(|n| n == &container.name).unwrap_or(false));

        match existing_ctr {
            Some(c) if c.state == running_v => return Ok(()),
            Some(_) if restart_policy == "Never" => return Ok(()),
            Some(c) => {
                // Not running and this pod is allowed to restart — clear the
                // stale container out so the create-below gets a fresh one.
                // Best-effort: if it's already gone by the time we ask, or
                // CRI won't remove it for some other reason, fall through
                // and let CreateContainer surface any real problem instead
                // of masking it here.
                let mut rt = self.rt.clone();
                let _ = rt.remove_container(RemoveContainerRequest { container_id: c.id.clone() }).await;
            }
            None => {}
        }

        let image = container.image.clone().unwrap_or_default();
        let image_spec = ImageSpec { image: image.clone(), ..Default::default() };

        // Pull the image (idempotent; containerd no-ops if present).
        let mut img = self.img.clone();
        img.pull_image(PullImageRequest {
            image: Some(image_spec.clone()),
            auth: None,
            sandbox_config: Some(sandbox_config(id)),
        })
        .await
        .context("pulling image")?;

        let mut rt = self.rt.clone();
        let config = ContainerConfig {
            metadata: Some(ContainerMetadata { name: container.name.clone(), attempt: 0 }),
            image: Some(image_spec),
            command: container.command.clone().unwrap_or_default(),
            args: container.args.clone().unwrap_or_default(),
            working_dir: container.working_dir.clone().unwrap_or_default(),
            envs: container
                .env
                .as_deref()
                .unwrap_or(&[])
                .iter()
                .map(|e| KeyValue {
                    key: e.name.clone(),
                    value: e.value.clone().unwrap_or_default().into_bytes(),
                })
                .collect(),
            labels: container_labels(id, &container.name),
            log_path: format!("{}_{}.log", container.name, 0),
            ..Default::default()
        };

        let created = rt
            .create_container(CreateContainerRequest {
                pod_sandbox_id: sandbox_id.to_string(),
                config: Some(config),
                sandbox_config: Some(sandbox_config(id)),
            })
            .await
            .context("creating container")?
            .into_inner();

        rt.start_container(StartContainerRequest { container_id: created.container_id })
            .await
            .context("starting container")?;
        Ok(())
    }

    async fn pod_ip(&self, sandbox_id: &str) -> Option<String> {
        let mut rt = self.rt.clone();
        let resp = rt
            .pod_sandbox_status(PodSandboxStatusRequest {
                pod_sandbox_id: sandbox_id.to_string(),
                verbose: false,
            })
            .await
            .ok()?
            .into_inner();
        let ip = resp.status?.network?.ip;
        (!ip.is_empty()).then_some(ip)
    }

    async fn build_status(&self, sandbox_id: &str, restart_policy: &str) -> Result<RuntimeStatus> {
        let containers = self.list_pod_containers(sandbox_id).await?;
        let running_v = ContainerState::ContainerRunning as i32;
        let exited_v = ContainerState::ContainerExited as i32;

        let mut crs = Vec::new();
        let mut any_running = false;
        let mut all_exited = !containers.is_empty();
        let mut earliest_created = i64::MAX;

        for c in &containers {
            let running = c.state == running_v;
            any_running |= running;
            all_exited &= c.state == exited_v;
            earliest_created = earliest_created.min(c.created_at);
            crs.push(ContainerRuntimeStatus {
                name: c.metadata.as_ref().map(|m| m.name.clone()).unwrap_or_default(),
                image: c.image.as_ref().map(|i| i.image.clone()).unwrap_or_default(),
                ready: running,
                running,
                container_id: Some(c.id.clone()),
            });
        }

        // all_exited only means "this pod is done" for restartPolicy: Never
        // (or OnFailure, treated the same here — the CRI status doesn't give
        // us per-container exit codes to distinguish "OnFailure but all
        // exited zero" from "OnFailure and one failed", and both still
        // report Pending/Succeeded reasonably either way). For the default,
        // overwhelmingly common restartPolicy: Always (every Deployment,
        // including coredns), a container exiting is never terminal —
        // ensure_container() above just restarted it — so report Pending
        // rather than Succeeded: never hand the ReplicaSet controller a
        // terminal phase for a pod that's still supposed to be alive.
        let phase = if any_running {
            Phase::Running
        } else if all_exited && restart_policy == "Never" {
            Phase::Succeeded
        } else {
            Phase::Pending
        };

        let started_at = (earliest_created != i64::MAX && earliest_created > 0)
            .then(|| Timestamp::from_nanosecond(earliest_created as i128).ok())
            .flatten();

        Ok(RuntimeStatus {
            phase,
            message: None,
            started_at,
            pod_ip: self.pod_ip(sandbox_id).await,
            containers: crs,
        })
    }
}

#[async_trait]
impl PodRuntime for CriRuntime {
    async fn ensure_pod(&self, pod: &Pod) -> Result<RuntimeStatus> {
        let id = pod_id(pod);
        let sandbox_id = match self.find_sandbox(&id.namespace, &id.name).await? {
            Some(s) => s,
            None => self.run_sandbox(&id).await.context("RunPodSandbox")?,
        };

        let restart_policy = pod
            .spec
            .as_ref()
            .and_then(|s| s.restart_policy.clone())
            .unwrap_or_else(|| "Always".to_string());
        // Recorded for status()'s event-driven path, which only gets
        // namespace+name (no Pod object) and needs this to make the same
        // Pending-vs-Succeeded call build_status() below does.
        self.restart_policies.lock().unwrap().insert(sandbox_id.clone(), restart_policy.clone());

        if let Some(spec) = pod.spec.as_ref() {
            for c in &spec.containers {
                self.ensure_container(&sandbox_id, &id, c, &restart_policy).await?;
            }
        }

        self.build_status(&sandbox_id, &restart_policy).await
    }

    async fn remove_pod(&self, namespace: &str, name: &str) -> Result<()> {
        if let Some(sandbox_id) = self.find_sandbox(namespace, name).await? {
            let mut rt = self.rt.clone();
            // StopPodSandbox is idempotent; RemovePodSandbox also removes its containers.
            let _ = rt
                .stop_pod_sandbox(StopPodSandboxRequest { pod_sandbox_id: sandbox_id.clone() })
                .await;
            rt.remove_pod_sandbox(RemovePodSandboxRequest { pod_sandbox_id: sandbox_id.clone() })
                .await
                .context("RemovePodSandbox")?;
            self.restart_policies.lock().unwrap().remove(&sandbox_id);
        }
        Ok(())
    }

    async fn status(&self, namespace: &str, name: &str) -> Result<Option<RuntimeStatus>> {
        match self.find_sandbox(namespace, name).await? {
            Some(sandbox_id) => {
                let restart_policy = self
                    .restart_policies
                    .lock()
                    .unwrap()
                    .get(&sandbox_id)
                    .cloned()
                    .unwrap_or_else(|| "Always".to_string());
                Ok(Some(self.build_status(&sandbox_id, &restart_policy).await?))
            }
            None => Ok(None),
        }
    }

    fn take_event_rx(&self) -> Option<UnboundedReceiver<String>> {
        self.rx.lock().unwrap().take()
    }
}

fn sandbox_labels(id: &PodId) -> HashMap<String, String> {
    HashMap::from([
        (POD_UID_LABEL.to_string(), id.uid.clone()),
        (POD_NAME_LABEL.to_string(), id.name.clone()),
        (POD_NS_LABEL.to_string(), id.namespace.clone()),
    ])
}

fn container_labels(id: &PodId, container_name: &str) -> HashMap<String, String> {
    let mut l = sandbox_labels(id);
    l.insert(CTR_NAME_LABEL.to_string(), container_name.to_string());
    l
}

fn sandbox_config(id: &PodId) -> PodSandboxConfig {
    // Host-network pods set the network namespace to NODE, which makes the CRI
    // runtime skip CNI entirely (no pod network to set up).
    let linux = id.host_network.then(|| LinuxPodSandboxConfig {
        security_context: Some(LinuxSandboxSecurityContext {
            namespace_options: Some(NamespaceOption {
                network: NamespaceMode::Node as i32,
                ..Default::default()
            }),
            ..Default::default()
        }),
        ..Default::default()
    });

    PodSandboxConfig {
        metadata: Some(PodSandboxMetadata {
            name: id.name.clone(),
            uid: id.uid.clone(),
            namespace: id.namespace.clone(),
            attempt: 0,
        }),
        // Host-network sandboxes share the host UTS namespace, so a hostname
        // cannot be set (runc rejects it). Real kubelets leave it empty too.
        hostname: if id.host_network { String::new() } else { id.name.clone() },
        log_directory: format!("/var/log/pods/{}_{}_{}", id.namespace, id.name, id.uid),
        labels: sandbox_labels(id),
        linux,
        ..Default::default()
    }
}

/// Event subscriber: prefer the CRI-standard `GetContainerEvents` (works on
/// containerd >= 1.7 and CRI-O); if the runtime doesn't implement it, fall back
/// to containerd's top-level `Events/Subscribe` API (present in every containerd
/// version). Either way, changed pod keys are pushed onto `tx` — no polling.
async fn event_loop(channel: Channel, tx: UnboundedSender<String>) {
    if run_cri_events(&channel, &tx).await == EventOutcome::Unsupported {
        info!("CRI GetContainerEvents unsupported; using containerd native events API");
        containerd_events_loop(channel, tx).await;
    }
}

#[derive(PartialEq)]
enum EventOutcome {
    Unsupported,
    ReceiverGone,
}

/// Returns `Unsupported` if the runtime lacks `GetContainerEvents` (caller should
/// fall back); otherwise keeps reconnecting and only returns when `tx` is closed.
async fn run_cri_events(channel: &Channel, tx: &UnboundedSender<String>) -> EventOutcome {
    loop {
        let mut client = RuntimeServiceClient::new(channel.clone());
        match client.get_container_events(GetEventsRequest::default()).await {
            Ok(resp) => {
                let mut stream = resp.into_inner();
                loop {
                    match stream.message().await {
                        Ok(Some(ev)) => {
                            if let Some(meta) = ev.pod_sandbox_status.and_then(|s| s.metadata) {
                                let key = super::pod_key(&meta.namespace, &meta.name);
                                debug!(pod = %key, "CRI container event");
                                if tx.send(key).is_err() {
                                    return EventOutcome::ReceiverGone;
                                }
                            }
                        }
                        Ok(None) => break, // stream ended; reconnect
                        Err(e) => {
                            warn!(error = ?e, "CRI event stream error");
                            break;
                        }
                    }
                }
            }
            Err(e) if e.code() == tonic::Code::Unimplemented => return EventOutcome::Unsupported,
            Err(e) => warn!(error = ?e, "failed to open CRI event stream"),
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

/// Fallback: subscribe to containerd's native event firehose in the `k8s.io`
/// namespace, watch `/tasks/*` events, map the container id back to a pod via its
/// labels, and push the pod key. Reconnects on error.
async fn containerd_events_loop(channel: Channel, tx: UnboundedSender<String>) {
    loop {
        let mut client = EventsClient::new(channel.clone());
        // Empty filters = whole firehose; we scope to k8s.io via the namespace
        // header and filter topics client-side (robust against filter grammar).
        let mut req = tonic::Request::new(SubscribeRequest { filters: vec![] });
        req.metadata_mut()
            .insert("containerd-namespace", "k8s.io".parse().unwrap());

        match client.subscribe(req).await {
            Ok(resp) => {
                let mut stream = resp.into_inner();
                loop {
                    match stream.message().await {
                        Ok(Some(env)) => {
                            if !env.topic.starts_with("/tasks/") {
                                continue;
                            }
                            let Some(cid) = env
                                .event
                                .and_then(|a| TaskEventContainerId::decode(a.value.as_slice()).ok())
                                .map(|t| t.container_id)
                                .filter(|c| !c.is_empty())
                            else {
                                continue;
                            };
                            debug!(topic = %env.topic, container = %cid, "containerd task event");
                            if let Some((ns, name)) = lookup_pod_by_cid(channel.clone(), &cid).await {
                                if tx.send(super::pod_key(&ns, &name)).is_err() {
                                    return; // controller dropped the receiver
                                }
                            }
                        }
                        Ok(None) => break,
                        Err(e) => {
                            warn!(error = ?e, "containerd event stream error");
                            break;
                        }
                    }
                }
            }
            Err(e) => warn!(error = ?e, "failed to subscribe to containerd events"),
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

/// Map a containerd container/sandbox id back to its pod (namespace, name) via
/// the `nodelet.dev/*` labels we stamped on it.
async fn lookup_pod_by_cid(channel: Channel, cid: &str) -> Option<(String, String)> {
    fn ns_name(labels: &HashMap<String, String>) -> Option<(String, String)> {
        Some((labels.get(POD_NS_LABEL)?.clone(), labels.get(POD_NAME_LABEL)?.clone()))
    }

    // App containers first.
    let mut rt = RuntimeServiceClient::new(channel.clone());
    if let Ok(resp) = rt
        .list_containers(ListContainersRequest {
            filter: Some(ContainerFilter { id: cid.to_string(), ..Default::default() }),
        })
        .await
    {
        if let Some(c) = resp.into_inner().containers.into_iter().next() {
            if let Some(p) = ns_name(&c.labels) {
                return Some(p);
            }
        }
    }

    // Otherwise the id may be a pod sandbox (e.g. the pause container's task).
    let mut rt = RuntimeServiceClient::new(channel);
    if let Ok(resp) = rt
        .list_pod_sandbox(ListPodSandboxRequest {
            filter: Some(PodSandboxFilter { id: cid.to_string(), ..Default::default() }),
        })
        .await
    {
        if let Some(s) = resp.into_inner().items.into_iter().next() {
            return ns_name(&s.labels);
        }
    }
    None
}
