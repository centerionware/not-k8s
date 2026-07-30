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
use serde_json::Value;
use std::sync::Arc;
use std::time::Duration;
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
                let prev = pod.status.as_ref();
                if let Err(e) = write_status(&self.client, &self.host_ip, &ns, &name, &status, prev).await {
                    warn!(pod = %format!("{ns}/{name}"), error = ?e, "failed to write pod status");
                }
            }
            Err(e) => {
                // A failed ensure_pod (e.g. a CNI plugin racing flanneld's own
                // startup — the exact failure that motivated this) previously
                // just sat there: this controller only reacts to watch/runtime
                // events, and an unchanged Pod generates neither, so nothing
                // would try again until the watch stream happened to reconnect
                // and relist. One bounded, targeted retry — not a periodic
                // poll — reacts to the failure itself as its own edge instead
                // of silently dropping it.
                warn!(pod = %format!("{ns}/{name}"), error = ?e, "ensure_pod failed; retrying once in 5s");
                self.schedule_retry(ns, name);
            }
        }
    }

    /// One delayed retry for a pod whose first ensure_pod failed. Runs
    /// detached from the reconcile loop (needs to outlive this call), so it
    /// re-fetches the Pod rather than reusing the possibly-stale one — if
    /// it's gone or being deleted by the time the retry fires, there's
    /// nothing to do. Deliberately doesn't retry again on a second failure:
    /// a persistent failure should surface as a stuck Pending pod for a
    /// human to look at, not retry forever in the background.
    fn schedule_retry(&self, ns: String, name: String) {
        let client = self.client.clone();
        let runtime = self.runtime.clone();
        let host_ip = self.host_ip.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(5)).await;
            let api: Api<Pod> = Api::namespaced(client.clone(), &ns);
            let pod = match api.get_opt(&name).await {
                Ok(Some(p)) if p.metadata.deletion_timestamp.is_none() => p,
                _ => return,
            };
            match runtime.ensure_pod(&pod).await {
                Ok(status) => {
                    debug!(pod = %format!("{ns}/{name}"), phase = status.phase.as_str(), "ensured (retry)");
                    let prev = pod.status.as_ref();
                    if let Err(e) = write_status(&client, &host_ip, &ns, &name, &status, prev).await {
                        warn!(pod = %format!("{ns}/{name}"), error = ?e, "failed to write pod status (retry)");
                    }
                }
                Err(e) => warn!(pod = %format!("{ns}/{name}"), error = ?e, "retry ensure_pod also failed — waiting for the next watch event instead of retrying indefinitely"),
            }
        });
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
            Ok(Some(p)) => {
                if let Err(e) = write_status(&self.client, &self.host_ip, ns, name, &status, p.status.as_ref()).await {
                    warn!(pod = %key, error = ?e, "failed to write pod status");
                } else {
                    debug!(pod = %key, phase = status.phase.as_str(), "status updated (event-driven)");
                }
            }
            Ok(None) => debug!(pod = %key, "pod gone; skipping status write"),
            Err(e) => warn!(pod = %key, error = ?e, "get_opt failed"),
        }
    }
}

/// Free functions (not PodController methods) so schedule_retry()'s
/// detached, 'static spawned task can call them without borrowing `self`.
async fn write_status(
    client: &Client,
    host_ip: &str,
    ns: &str,
    name: &str,
    rt: &RuntimeStatus,
    prev: Option<&PodStatus>,
) -> Result<()> {
    let api: Api<Pod> = Api::namespaced(client.clone(), ns);
    let status = build_pod_status(host_ip, rt, prev);
    if !status_patch_changes(prev, &status) {
        debug!(pod = %format!("{ns}/{name}"), "skipped unchanged pod status patch");
        return Ok(());
    }
    let patch = serde_json::json!({ "status": status });
    api.patch_status(name, &PatchParams::default(), &Patch::Merge(&patch)).await?;
    Ok(())
}

/// Return whether merging `desired` into the stored PodStatus would change
/// anything. `patch_status()` uses JSON Merge Patch: object fields merge
/// recursively, arrays replace as a whole, and null deletes a field. Checking
/// this before the HTTP call matters because an identical PATCH can still
/// produce a new resourceVersion and feed the same Pod back into our watch.
fn status_patch_changes(prev: Option<&PodStatus>, desired: &PodStatus) -> bool {
    let Some(prev) = prev else { return true };
    let prev = serde_json::to_value(prev).expect("PodStatus must serialize");
    let desired = serde_json::to_value(desired).expect("PodStatus must serialize");
    merge_patch_changes(Some(&prev), &desired)
}

fn merge_patch_changes(current: Option<&Value>, patch: &Value) -> bool {
    match patch {
        Value::Object(patch) => {
            let current = current.and_then(Value::as_object);
            patch.iter().any(|(key, value)| match value {
                Value::Null => current.and_then(|obj| obj.get(key)).is_some(),
                value => merge_patch_changes(current.and_then(|obj| obj.get(key)), value),
            })
        }
        value => current != Some(value),
    }
}

/// `prev` is the pod's previously-stored status (straight from the watch
/// event / a fresh get), used only to carry `last_transition_time` forward
/// for conditions whose status hasn't actually changed. Stamping every
/// condition with `now()` on every call — the previous behavior — made every
/// patch_status a real diff, which bumps resourceVersion, which the
/// apiserver delivers back to nodelet's own field-selected watch as a fresh
/// Apply event, which re-triggers reconcile(), which calls write_status()
/// again: an unbounded self-feedback loop hammering both the CRI runtime and
/// the apiserver for every pod, forever, regardless of whether anything
/// real changed. Preserving unchanged timestamps makes an unchanged status
/// patch a true no-op (identical to the stored object), so the apiserver
/// doesn't bump resourceVersion and the loop breaks.
fn build_pod_status(host_ip: &str, rt: &RuntimeStatus, prev: Option<&PodStatus>) -> PodStatus {
    let running = rt.phase == Phase::Running;
    let prev_time = |type_: &str, status: &str| -> Option<Time> {
        prev?
            .conditions
            .as_ref()?
            .iter()
            .find(|c| c.type_ == type_ && c.status == status)
            .and_then(|c| c.last_transition_time.clone())
    };
    let cond = |type_: &str, ok: bool| {
        let status = if ok { "True" } else { "False" }.to_string();
        let last_transition_time = prev_time(type_, &status)
            .or_else(|| Some(Time(k8s_openapi::jiff::Timestamp::now())));
        PodCondition { type_: type_.to_string(), status, last_transition_time, ..Default::default() }
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
        host_ip: Some(host_ip.to_string()),
        pod_ip: rt.pod_ip.clone(),
        pod_ips,
        start_time: rt.started_at.map(Time),
        message: rt.message.clone(),
        ..Default::default()
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

// Small, isolated test files — one behavior area each.
#[cfg(test)]
#[path = "pods_tests/build_pod_status.rs"]
mod tests_build_pod_status;
#[cfg(test)]
#[path = "pods_tests/key_parts.rs"]
mod tests_key_parts;
#[cfg(test)]
#[path = "pods_tests/event_loop.rs"]
mod tests_event_loop;
