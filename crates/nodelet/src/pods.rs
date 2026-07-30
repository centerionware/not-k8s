//! The pod controller.
//!
//! Watches only the Pods bound to this node (`fieldSelector spec.nodeName=...`),
//! reconciles them against the pluggable runtime, and writes back `Pod.status`.
//!
//! Two event sources drive a single `select!` loop — and that's the whole design:
//!   * the apiserver **watch** stream (desired state changes), and
//!   * the runtime **event** channel (actual state changes).
//! There is no periodic relist and no per-second polling (no PLEG). We react to
//! edges, then reconcile the one pod that changed.

use crate::runtime::{Phase, PodRuntime, RuntimeStatus};
use anyhow::Result;
use futures::StreamExt;
use k8s_openapi::api::core::v1::{
    ContainerState, ContainerStateRunning, ContainerStateWaiting, ContainerStatus, Pod,
    PodCondition, PodIP, PodStatus,
};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::Time;
use kube::api::{Patch, PatchParams};
use kube::runtime::watcher;
use kube::runtime::watcher::Event;
use kube::{Api, Client};
use std::sync::Arc;
use tokio::sync::mpsc::UnboundedReceiver;
use tracing::{debug, info, warn};

pub struct PodController {
    client: Client,
    runtime: Arc<dyn PodRuntime>,
    node_name: String,
    host_ip: String,
    events: Option<UnboundedReceiver<String>>,
}

impl PodController {
    pub fn new(client: Client, runtime: Arc<dyn PodRuntime>, node_name: String) -> Self {
        let host_ip = crate::node::detect_internal_ip();
        let events = runtime.take_event_rx();
        Self { client, runtime, node_name, host_ip, events }
    }

    /// Run the reconcile loop. Returns only if the watch stream terminates;
    /// the caller may call again to restart (the event receiver is retained).
    pub async fn run(&mut self) -> Result<()> {
        let api: Api<Pod> = Api::all(self.client.clone());
        let wc = watcher::Config::default()
            .fields(&format!("spec.nodeName={}", self.node_name));
        let mut stream = watcher(api, wc).boxed();
        // Move the receiver into a local so reconcile methods can borrow `&self`.
        let mut events = self.events.take();

        info!(node = %self.node_name, "pod controller watching pods bound to this node");

        loop {
            tokio::select! {
                key = next_event(&mut events) => {
                    self.on_runtime_event(&key).await;
                }
                item = stream.next() => {
                    match item {
                        Some(Ok(ev)) => self.on_watch(ev).await,
                        Some(Err(e)) => warn!(error = ?e, "pod watch error; watcher will retry"),
                        None => {
                            warn!("pod watch stream ended; restarting");
                            self.events = events; // retain for the next run()
                            return Ok(());
                        }
                    }
                }
            }
        }
    }

    async fn on_watch(&self, ev: Event<Pod>) {
        match ev {
            Event::Apply(pod) | Event::InitApply(pod) => self.reconcile(pod).await,
            Event::Delete(pod) => self.teardown(&pod).await,
            Event::Init | Event::InitDone => {}
        }
    }

    /// Drive a single pod toward its desired state.
    async fn reconcile(&self, pod: Pod) {
        let (ns, name) = match key_parts(&pod) {
            Some(v) => v,
            None => return,
        };

        if pod.metadata.deletion_timestamp.is_some() {
            self.teardown(&pod).await;
            return;
        }

        match self.runtime.ensure_pod(&pod).await {
            Ok(status) => {
                debug!(pod = %format!("{ns}/{name}"), phase = status.phase.as_str(), "ensured");
                if let Err(e) = self.write_status(&ns, &name, &status).await {
                    warn!(pod = %format!("{ns}/{name}"), error = ?e, "failed to write pod status");
                }
            }
            Err(e) => warn!(pod = %format!("{ns}/{name}"), error = ?e, "ensure_pod failed"),
        }
    }

    async fn teardown(&self, pod: &Pod) {
        if let Some((ns, name)) = key_parts(pod) {
            if let Err(e) = self.runtime.remove_pod(&ns, &name).await {
                warn!(pod = %format!("{ns}/{name}"), error = ?e, "remove_pod failed");
            } else {
                info!(pod = %format!("{ns}/{name}"), "torn down");
            }
        }
    }

    /// Runtime told us a pod's actual state changed — reconcile just its status.
    async fn on_runtime_event(&self, key: &str) {
        let Some((ns, name)) = key.split_once('/') else { return };
        let status = match self.runtime.status(ns, name).await {
            Ok(Some(s)) => s,
            Ok(None) => return,
            Err(e) => {
                warn!(pod = %key, error = ?e, "runtime status query failed");
                return;
            }
        };
        // Only write if the pod still exists in the apiserver.
        let api: Api<Pod> = Api::namespaced(self.client.clone(), ns);
        match api.get_opt(name).await {
            Ok(Some(_)) => {
                if let Err(e) = self.write_status(ns, name, &status).await {
                    warn!(pod = %key, error = ?e, "failed to write pod status");
                } else {
                    debug!(pod = %key, phase = status.phase.as_str(), "status updated (event-driven)");
                }
            }
            Ok(None) => debug!(pod = %key, "pod gone; skipping status write"),
            Err(e) => warn!(pod = %key, error = ?e, "get_opt failed"),
        }
    }

    async fn write_status(&self, ns: &str, name: &str, rt: &RuntimeStatus) -> Result<()> {
        let api: Api<Pod> = Api::namespaced(self.client.clone(), ns);
        let status = self.build_pod_status(rt);
        let patch = serde_json::json!({ "status": status });
        api.patch_status(name, &PatchParams::default(), &Patch::Merge(&patch)).await?;
        Ok(())
    }

    fn build_pod_status(&self, rt: &RuntimeStatus) -> PodStatus {
        let running = rt.phase == Phase::Running;
        let cond = |type_: &str, ok: bool| PodCondition {
            type_: type_.to_string(),
            status: if ok { "True" } else { "False" }.to_string(),
            last_transition_time: Some(Time(k8s_openapi::jiff::Timestamp::now())),
            ..Default::default()
        };

        let container_statuses = rt
            .containers
            .iter()
            .map(|c| ContainerStatus {
                name: c.name.clone(),
                image: c.image.clone(),
                image_id: String::new(),
                ready: c.ready,
                restart_count: 0,
                started: Some(c.running),
                container_id: c.container_id.clone(),
                state: Some(if c.running {
                    ContainerState {
                        running: Some(ContainerStateRunning {
                            started_at: rt.started_at.map(Time),
                        }),
                        ..Default::default()
                    }
                } else {
                    ContainerState {
                        waiting: Some(ContainerStateWaiting {
                            reason: Some("ContainerCreating".to_string()),
                            message: None,
                        }),
                        ..Default::default()
                    }
                }),
                ..Default::default()
            })
            .collect();

        let pod_ips = rt
            .pod_ip
            .as_ref()
            .map(|ip| vec![PodIP { ip: ip.clone() }]);

        PodStatus {
            phase: Some(rt.phase.as_str().to_string()),
            conditions: Some(vec![
                cond("Initialized", true),
                cond("PodScheduled", true),
                cond("ContainersReady", running),
                cond("Ready", running),
            ]),
            container_statuses: Some(container_statuses),
            host_ip: Some(self.host_ip.clone()),
            pod_ip: rt.pod_ip.clone(),
            pod_ips,
            start_time: rt.started_at.map(Time),
            message: rt.message.clone(),
            ..Default::default()
        }
    }
}

/// Park forever if there is no event channel; otherwise yield the next key.
/// On channel close, drop the receiver and park (watch stream keeps the loop alive).
async fn next_event(events: &mut Option<UnboundedReceiver<String>>) -> String {
    match events {
        Some(rx) => match rx.recv().await {
            Some(key) => key,
            None => {
                *events = None;
                std::future::pending().await
            }
        },
        None => std::future::pending().await,
    }
}

fn key_parts(pod: &Pod) -> Option<(String, String)> {
    let ns = pod.metadata.namespace.clone().unwrap_or_else(|| "default".to_string());
    let name = pod.metadata.name.clone()?;
    Some((ns, name))
}
