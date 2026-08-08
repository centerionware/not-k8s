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

use crate::probes::{self, HealthMap};
use crate::runtime::{Phase, PodRuntime, RuntimeStatus};
use anyhow::Result;
use futures::StreamExt;
use k8s_openapi::api::core::v1::{
    ConfigMap, ContainerState, ContainerStateRunning, ContainerStateTerminated, ContainerStateWaiting,
    ContainerStatus, ContainerUser, HostIP, LinuxContainerUser, Pod, PodCondition, PodIP, PodStatus, ResourceHealth,
    ResourceStatus, Secret, Volume, VolumeMountStatus,
};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::Time;
use kube::api::{DeleteParams, ListParams, Patch, PatchParams};
use kube::runtime::watcher;
use kube::runtime::watcher::Event;
use kube::{Api, Client};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

pub struct PodController {
    client: Client,
    runtime: Arc<dyn PodRuntime>,
    node_name: String,
    host_ip: String,
    events: Option<UnboundedReceiver<String>>,
    /// Round 123: a probe transitioning readiness/started in `health` is a
    /// real state change (it flips `status.conditions[Ready]` and
    /// `containerStatuses[].ready/started`) but, unlike a container
    /// restart or exit, it never touches the apiserver or the CRI
    /// runtime's own event stream on its own — nothing else in this
    /// select! loop would ever notice it happened. Found live: a plain
    /// readinessProbe with no restarts anywhere in its pod's lifetime
    /// (the common case) got stuck reporting `Ready: False` forever after
    /// the one-time initial reconcile, because nothing ever re-ran
    /// write_status() to pick up the health map's new value. This
    /// channel is probes.rs's own way to say "re-check this pod's status
    /// now" — same handler (`on_runtime_event`) the CRI event channel
    /// already uses, since it already reads `self.health` on every call.
    probe_events_tx: UnboundedSender<String>,
    probe_events_rx: Option<UnboundedReceiver<String>>,
    /// Per-container liveness/readiness state, written by probe supervisor
    /// tasks and read by build_pod_status(). Shared (not owned per-pod)
    /// because build_pod_status() is a free function reachable from the
    /// detached schedule_retry() task too.
    health: HealthMap,
    /// One probe-supervisor task per pod key, so re-reconciling an
    /// unchanged pod doesn't spawn duplicates. Aborted on teardown.
    probe_tasks: Mutex<HashMap<String, Vec<JoinHandle<()>>>>,
    /// Pod UIDs already torn down (see `reconcile()`'s deletion branch) —
    /// keyed by UID rather than namespace/name, so a same-named pod
    /// recreated after this one is genuinely gone still gets a real
    /// teardown. Entries are removed once the real `Event::Delete`
    /// confirms the object actually left the apiserver, so this stays
    /// bounded by "pods currently mid-termination", not "every pod this
    /// node has ever run."
    torn_down: Mutex<HashSet<String>>,
}

impl PodController {
    pub fn new(client: Client, runtime: Arc<dyn PodRuntime>, node_name: String) -> Self {
        let host_ip = crate::node::detect_internal_ip();
        let events = runtime.take_event_rx();
        let (probe_events_tx, probe_events_rx) = mpsc::unbounded_channel();
        Self {
            client,
            runtime,
            node_name,
            host_ip,
            events,
            probe_events_tx,
            probe_events_rx: Some(probe_events_rx),
            health: probes::new_health_map(),
            probe_tasks: Mutex::new(HashMap::new()),
            torn_down: Mutex::new(HashSet::new()),
        }
    }

    /// Spawn a probe supervisor for this pod if it declares any probes and
    /// one isn't already running for it. Idempotent per pod key — cheap to
    /// call from every reconcile().
    fn ensure_probe_supervisor(&self, pod: &Pod, ns: &str, name: &str, pod_ip: Option<&str>) {
        let key = crate::runtime::pod_key(ns, name);
        let mut containers = pod.spec.as_ref().map(|s| s.containers.clone()).unwrap_or_default();
        // Native sidecar containers (round 36) get the same liveness/
        // readiness/startup probe supervision an app container does —
        // they run for the pod's whole lifetime, unlike a regular
        // (one-shot) init container. `probes::spawn()`/`restart_container()`
        // both look a container up by name alone (no init/app
        // distinction anywhere in that path), so this just works by
        // adding them to the same list.
        if let Some(init_containers) = pod.spec.as_ref().and_then(|s| s.init_containers.as_ref()) {
            containers.extend(init_containers.iter().filter(|c| c.restart_policy.as_deref() == Some("Always")).cloned());
        }
        if !probes::has_any_probe(&containers) {
            return;
        }
        let Some(pod_ip) = pod_ip else { return }; // no IP yet; wait for the next reconcile
        let mut tasks = self.probe_tasks.lock().unwrap();
        if tasks.contains_key(&key) {
            return;
        }
        // Round 44 (found in round 35's re-audit): the pod's own
        // terminationGracePeriodSeconds, used when a liveness probe's own
        // override isn't set — same default (30) real kubelet applies.
        let pod_grace_period_seconds =
            pod.spec.as_ref().and_then(|s| s.termination_grace_period_seconds).filter(|s| *s >= 0).unwrap_or(30);
        let handles = probes::spawn(
            self.runtime.clone(),
            self.client.clone(),
            self.health.clone(),
            self.probe_events_tx.clone(),
            ns.to_string(),
            name.to_string(),
            containers,
            pod_ip.to_string(),
            pod_grace_period_seconds,
        );
        tasks.insert(key, handles);
    }

    fn stop_probe_supervisor(&self, ns: &str, name: &str) {
        let key = crate::runtime::pod_key(ns, name);
        // Every container's probe loop is its own independent tokio task
        // (see probes::spawn()'s doc comment) — abort() doesn't cascade, so
        // all of them must be aborted individually or the old ones leak and
        // keep restarting containers on their own schedule forever.
        if let Some(handles) = self.probe_tasks.lock().unwrap().remove(&key) {
            for handle in handles {
                handle.abort();
            }
        }
        probes::clear_pod_health(&self.health, ns, name);
    }

    /// Run the reconcile loop. Returns only if the watch stream terminates;
    /// the caller may call again to restart (the event receiver is retained).
    pub async fn run(&mut self) -> Result<()> {
        let api: Api<Pod> = Api::all(self.client.clone());
        let wc = watcher::Config::default()
            .fields(&format!("spec.nodeName={}", self.node_name));
        let mut stream = watcher(api, wc).boxed();
        // ConfigMaps/Secrets have no node-scoping fieldSelector (they aren't
        // bound to a node), so these watches are cluster-wide. That's fine:
        // they're rare-write objects and we only react to edges, never poll.
        let cm_api: Api<ConfigMap> = Api::all(self.client.clone());
        let mut cm_stream = watcher(cm_api, watcher::Config::default()).boxed();
        let sec_api: Api<Secret> = Api::all(self.client.clone());
        let mut sec_stream = watcher(sec_api, watcher::Config::default()).boxed();
        // Move the receivers into locals so reconcile methods can borrow `&self`.
        let mut events = self.events.take();
        let mut probe_events = self.probe_events_rx.take();

        info!(node = %self.node_name, "pod controller watching pods bound to this node");

        loop {
            tokio::select! {
                key = next_event(&mut events) => {
                    self.on_runtime_event(&key).await;
                }
                key = next_event(&mut probe_events) => {
                    self.on_runtime_event(&key).await;
                }
                item = stream.next() => {
                    match item {
                        Some(Ok(ev)) => self.on_watch(ev).await,
                        Some(Err(e)) => warn!(error = ?e, "pod watch error; watcher will retry"),
                        None => {
                            warn!("pod watch stream ended; restarting");
                            self.events = events; self.probe_events_rx = probe_events; // retain for the next run()
                            return Ok(());
                        }
                    }
                }
                item = cm_stream.next() => {
                    match item {
                        Some(Ok(Event::Apply(cm))) | Some(Ok(Event::InitApply(cm))) => {
                            if let (Some(ns), Some(name)) = (cm.metadata.namespace.clone(), cm.metadata.name.clone()) {
                                self.on_referenced_object_changed(&ns, &name, ReferencedKind::ConfigMap).await;
                            }
                        }
                        Some(Ok(_)) => {}
                        Some(Err(e)) => warn!(error = ?e, "configmap watch error; watcher will retry"),
                        None => {
                            warn!("configmap watch stream ended; restarting");
                            self.events = events; self.probe_events_rx = probe_events;
                            return Ok(());
                        }
                    }
                }
                item = sec_stream.next() => {
                    match item {
                        Some(Ok(Event::Apply(sec))) | Some(Ok(Event::InitApply(sec))) => {
                            if let (Some(ns), Some(name)) = (sec.metadata.namespace.clone(), sec.metadata.name.clone()) {
                                self.on_referenced_object_changed(&ns, &name, ReferencedKind::Secret).await;
                            }
                        }
                        Some(Ok(_)) => {}
                        Some(Err(e)) => warn!(error = ?e, "secret watch error; watcher will retry"),
                        None => {
                            warn!("secret watch stream ended; restarting");
                            self.events = events; self.probe_events_rx = probe_events;
                            return Ok(());
                        }
                    }
                }
            }
        }
    }

    /// A ConfigMap or Secret changed. Re-reconcile every pod on this node
    /// whose volumes reference it, so the bind-mounted files get fresh
    /// content within seconds — no pod/container restart needed, matching
    /// real kubelet's config-map-manager live-update behavior. Env vars
    /// (envFrom/valueFrom) are deliberately NOT covered: kubelet captures
    /// those once at container start, by design, and never refreshes them.
    async fn on_referenced_object_changed(&self, namespace: &str, name: &str, kind: ReferencedKind) {
        let api: Api<Pod> = Api::namespaced(self.client.clone(), namespace);
        let lp = ListParams::default().fields(&format!("spec.nodeName={}", self.node_name));
        let pods = match api.list(&lp).await {
            Ok(list) => list.items,
            Err(e) => {
                warn!(namespace, name, ?kind, error = ?e, "failed to list pods while handling ConfigMap/Secret change");
                return;
            }
        };
        for pod in pods {
            let referenced = match kind {
                ReferencedKind::ConfigMap => referenced_configmap_names(&pod).contains(name),
                ReferencedKind::Secret => referenced_secret_names(&pod).contains(name),
            };
            if !referenced {
                continue;
            }
            if let Some((ns, pname)) = key_parts(&pod) {
                info!(pod = %format!("{ns}/{pname}"), namespace, name, ?kind, "re-materializing volumes after referenced object changed");
            }
            self.reconcile(pod).await;
        }
    }

    async fn on_watch(&self, ev: Event<Pod>) {
        match ev {
            Event::Apply(pod) | Event::InitApply(pod) => self.reconcile(pod).await,
            Event::Delete(pod) => {
                // The real, final removal — always run teardown() here
                // (idempotent-safe: everything it does already tolerates
                // "target already gone" with a warn, not a hard failure),
                // then forget the UID: the object genuinely left the
                // apiserver, so there's nothing left to guard against
                // re-tearing-down.
                self.teardown(&pod).await;
                if let Some(uid) = pod.metadata.uid.as_deref() {
                    self.torn_down.lock().unwrap().remove(uid);
                }
            }
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
            // A finalizer-blocked pod (or just ordinary apiserver
            // propagation delay) can generate several more watch events
            // for the SAME still-terminating object before it's actually
            // gone (any status settling, a resync, etc.) — reconcile()
            // dispatches on deletion_timestamp alone, so without this
            // check every one of those re-ran the full teardown() (real
            // network round-trips: CSI unmount RPCs, PVC re-fetches)
            // instead of a no-op. Found live (round 123): this single
            // serial watch-event loop processes one event at a time, so a
            // burst of redundant teardown() calls for one pod could delay
            // it from reaching a completely unrelated pod's own creation
            // event — a real, if unconfirmed, contributor to pods
            // intermittently taking far longer than expected to reach
            // Running in CI.
            // No UID at all is a real apiserver-watch-event anomaly, not
            // something normal to dedupe against — fall through to a
            // real teardown() every time rather than risk collapsing
            // unrelated events into one shared "" key.
            if let Some(uid) = pod.metadata.uid.clone() {
                if !self.torn_down.lock().unwrap().insert(uid) {
                    return; // already handled this pod's teardown
                }
            }
            self.teardown(&pod).await;
            return;
        }

        // A pod whose phase is already terminal (Failed/Succeeded) is
        // done for good — real Kubernetes semantics: once a pod reaches
        // Failed or Succeeded, its phase never goes back, and kubelet
        // must not (re)create/restart its containers regardless of
        // restartPolicy. Needed once evict_pod() (main.rs's
        // eviction_loop()) stopped deleting evicted/deadline-exceeded
        // pods outright (round 123, matching real kubelet — it never
        // deletes them either): evict_pod() stops containers and patches
        // status to Failed without deleting the object, which generates
        // a fresh watch event: without this check, the very next
        // reconcile would see "no containers running" for a
        // restartPolicy: Always pod and recreate them right back.
        // Also doubles as this pod's probe-supervisor cleanup, which
        // teardown() would otherwise have been the only path to.
        if matches!(pod.status.as_ref().and_then(|s| s.phase.as_deref()), Some("Failed") | Some("Succeeded")) {
            self.stop_probe_supervisor(&ns, &name);
            return;
        }

        match self.runtime.ensure_pod(&pod).await {
            Ok(status) => {
                debug!(pod = %format!("{ns}/{name}"), phase = status.phase.as_str(), "ensured");
                self.ensure_probe_supervisor(&pod, &ns, &name, status.pod_ip.as_deref());
                let prev = pod.status.as_ref();
                let gates = readiness_gate_types(&pod);
                let qos = crate::eviction::qos_class(&pod);
                if let Err(e) = write_status(&self.client, &self.host_ip, &ns, &name, &status, prev, &gates, &self.health, qos, pod.metadata.generation).await {
                    warn!(pod = %format!("{ns}/{name}"), error = ?e, "failed to write pod status");
                }
                // Round 124: a still-pending CSI volume attach or device-
                // plugin resource (pending_csi_volume_names()/pending_
                // device_plugin_resources()) touches nothing on the Pod
                // object itself, so — unlike almost every other state
                // change this controller reacts to — there's no future
                // watch event to wait for. Without an explicit retry
                // here, the pod would simply stay Pending forever once
                // the external attacher/plugin's own timing lost the
                // race with this reconcile. schedule_retry() already
                // exists for exactly this "nothing else will retry this"
                // shape (originally for a failed ensure_pod); it now also
                // recognizes and chains on both these conditions — see
                // its own doc comment.
                if is_waiting_for_external_resource(&status) {
                    self.schedule_retry(ns, name);
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

    /// A delayed retry for a pod whose first ensure_pod failed, OR (round
    /// 124) is waiting on a still-pending CSI volume attach or device
    /// plugin resource (`is_waiting_for_external_resource()`). Runs detached from the reconcile
    /// loop (needs to outlive this call), so it re-fetches the Pod rather
    /// than reusing the possibly-stale one — if it's gone or being deleted
    /// by the time a retry fires, there's nothing to do.
    ///
    /// The two cases get different persistence on purpose. A generic
    /// ensure_pod() *error* stays one-shot: a persistent failure should
    /// surface as a stuck Pending pod for a human to look at, not retry
    /// forever in the background. A pending CSI volume is different — it's
    /// not a failure at all, just an ordinary wait on an external-attacher
    /// that can legitimately take well over a minute under load (confirmed
    /// live in CI), so this keeps re-polling on the same 5s cadence for as
    /// long as that specific condition holds, bounded so a genuinely wedged
    /// attach still eventually stops retrying instead of polling forever.
    fn schedule_retry(&self, ns: String, name: String) {
        let client = self.client.clone();
        let runtime = self.runtime.clone();
        let host_ip = self.host_ip.clone();
        let health = self.health.clone();
        tokio::spawn(async move {
            const MAX_VOLUME_ATTEMPTS: u32 = 24; // ~2 minutes at 5s apart
            for attempt in 1..=MAX_VOLUME_ATTEMPTS {
                tokio::time::sleep(Duration::from_secs(5)).await;
                let api: Api<Pod> = Api::namespaced(client.clone(), &ns);
                let pod = match api.get_opt(&name).await {
                    Ok(Some(p)) if p.metadata.deletion_timestamp.is_none() => p,
                    _ => return,
                };
                match runtime.ensure_pod(&pod).await {
                    Ok(status) => {
                        debug!(pod = %format!("{ns}/{name}"), phase = status.phase.as_str(), attempt, "ensured (retry)");
                        let prev = pod.status.as_ref();
                        let gates = readiness_gate_types(&pod);
                        let qos = crate::eviction::qos_class(&pod);
                        if let Err(e) = write_status(&client, &host_ip, &ns, &name, &status, prev, &gates, &health, qos, pod.metadata.generation).await {
                            warn!(pod = %format!("{ns}/{name}"), error = ?e, "failed to write pod status (retry)");
                        }
                        if !is_waiting_for_external_resource(&status) {
                            return; // resolved, or failed for some other reason — either way, done retrying
                        }
                        if attempt == MAX_VOLUME_ATTEMPTS {
                            warn!(pod = %format!("{ns}/{name}"), "still waiting for a CSI volume attach after {MAX_VOLUME_ATTEMPTS} retries — giving up; pod stays Pending until the next real watch event");
                        }
                    }
                    Err(e) => {
                        warn!(pod = %format!("{ns}/{name}"), error = ?e, "retry ensure_pod also failed — waiting for the next watch event instead of retrying indefinitely");
                        return;
                    }
                }
            }
        });
    }

    /// Tears down a pod's runtime state and, if it still exists in the
    /// apiserver (the `reconcile()` path — `deletionTimestamp` set but not
    /// yet purged), finishes the deletion there too.
    ///
    /// Real kubelet's graceful-delete contract: `kubectl delete pod` only
    /// *sets* `deletionTimestamp` (a soft delete — the object stays in
    /// etcd). Nothing purges it from there on its own; the apiserver
    /// leaves that to whoever is actually terminating the pod. kubelet
    /// does this itself once termination finishes, by issuing a second,
    /// now-effectively-immediate `Delete`. Without that second call here,
    /// every normally-deleted pod would tear down its containers just
    /// fine but sit in `Terminating` forever — and so would any namespace
    /// containing it, since namespace GC waits for all its objects to be
    /// gone.
    ///
    /// `Event::Delete` (object already gone from etcd — someone else, or
    /// a previous call to this same function, already finished it) is the
    /// other caller; the delete call below tolerates a 404 as success so
    /// this stays a no-op harmless double-call in that case.
    async fn teardown(&self, pod: &Pod) {
        if let Some((ns, name)) = key_parts(pod) {
            self.stop_probe_supervisor(&ns, &name);
            if let Err(e) = self.runtime.remove_pod(pod).await {
                warn!(pod = %format!("{ns}/{name}"), error = ?e, "remove_pod failed");
                return;
            }
            info!(pod = %format!("{ns}/{name}"), "torn down");

            let api: Api<Pod> = Api::namespaced(self.client.clone(), &ns);
            let dp = DeleteParams { grace_period_seconds: Some(0), ..Default::default() };
            match api.delete(&name, &dp).await {
                Ok(_) => {}
                Err(kube::Error::Api(e)) if e.code == 404 => {}
                Err(e) => warn!(pod = %format!("{ns}/{name}"), error = ?e, "final delete of pod object failed"),
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
                self.ensure_probe_supervisor(&p, ns, name, status.pod_ip.as_deref());
                let gates = readiness_gate_types(&p);
                let qos = crate::eviction::qos_class(&p);
                if let Err(e) = write_status(
                    &self.client,
                    &self.host_ip,
                    ns,
                    name,
                    &status,
                    p.status.as_ref(),
                    &gates,
                    &self.health,
                    qos,
                    p.metadata.generation,
                )
                .await
                {
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
pub(crate) async fn write_status(
    client: &Client,
    host_ip: &str,
    ns: &str,
    name: &str,
    rt: &RuntimeStatus,
    prev: Option<&PodStatus>,
    readiness_gates: &[String],
    health: &HealthMap,
    qos: crate::eviction::QosClass,
    generation: Option<i64>,
) -> Result<()> {
    let api: Api<Pod> = Api::namespaced(client.clone(), ns);
    let status = build_pod_status(host_ip, ns, name, rt, prev, readiness_gates, health, qos, generation);
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
/// `spec.readinessGates`' condition types, as plain strings — pulled out
/// so `build_pod_status()`'s readiness computation doesn't need the whole
/// `Pod` object, just the piece of it that's actually relevant.
pub(crate) fn readiness_gate_types(pod: &Pod) -> Vec<String> {
    pod.spec
        .as_ref()
        .and_then(|s| s.readiness_gates.as_ref())
        .map(|gates| gates.iter().map(|g| g.condition_type.clone()).collect())
        .unwrap_or_default()
}

/// The condition types `build_pod_status()` computes and owns itself —
/// used to decide which conditions from `prev` (see `build_pod_status()`)
/// are foreign and must be copied forward rather than dropped.
const OWNED_CONDITION_TYPES: [&str; 5] =
    ["Initialized", "PodScheduled", "ContainersReady", "Ready", "PodResizeInProgress"];

/// Whether `condition_type` is currently reported `True` in `conditions` —
/// pure, what a `readinessGates` entry needs to check against an external
/// controller's condition.
fn condition_is_true(conditions: &[PodCondition], condition_type: &str) -> bool {
    conditions.iter().any(|c| c.type_ == condition_type && c.status == "True")
}

/// One container's `ContainerState` — `Running`, `Terminated` (round 24:
/// `c.exit_code.is_some()` means it has genuinely exited at least once,
/// as opposed to never having started at all), or `Waiting` with
/// `waiting_reason` (`"ContainerCreating"` for app containers,
/// `"PodInitializing"` for init containers — the two real kubelet uses)
/// for a container that's never run. `started_at` is only meaningful for
/// the `Running` case (`RuntimeStatus.started_at` is pod-wide, not
/// per-container — init containers pass `None` since they don't track
/// this individually, matching pre-round-24 behavior).
fn container_state(c: &crate::runtime::ContainerRuntimeStatus, started_at: Option<Time>, waiting_reason: &str) -> ContainerState {
    if c.running {
        ContainerState { running: Some(ContainerStateRunning { started_at }), ..Default::default() }
    } else if let Some(exit_code) = c.exit_code {
        ContainerState {
            terminated: Some(ContainerStateTerminated {
                container_id: c.container_id.clone(),
                exit_code,
                finished_at: c.finished_at.map(Time),
                message: (!c.termination_message.is_empty()).then(|| c.termination_message.clone()),
                reason: (!c.reason.is_empty()).then(|| c.reason.clone()),
                ..Default::default()
            }),
            ..Default::default()
        }
    } else {
        // Crash-loop backoff (round 75; found in round 73's own work):
        // `waiting_reason_override` (currently only ever
        // `"CrashLoopBackOff"`) takes priority over the caller's default
        // — this is the one case where a container that HAS actually run
        // before (and has a `lastState` to show) still reports `Waiting`
        // rather than `Terminated`, matching real kubelet's own display.
        let reason = c.waiting_reason_override.as_deref().unwrap_or(waiting_reason);
        ContainerState {
            waiting: Some(ContainerStateWaiting { reason: Some(reason.to_string()), message: None }),
            ..Default::default()
        }
    }
}

/// `PodCondition.observedGeneration` (round 87; found in round 86's
/// re-audit) — real kubelet's own `podutil.CalculatePodConditionObservedGeneration`:
/// a condition whose status is unchanged from the previous write keeps
/// its *old* `observedGeneration` (it was already correctly stamped
/// whenever it last actually changed); one that's new or whose status
/// just flipped gets stamped with the pod's *current*
/// `metadata.generation`. Lets a client tell whether a condition
/// reflects the pod's current spec generation or a stale one from
/// before the most recent spec update.
fn condition_observed_generation(prev: Option<&PodCondition>, new_status: &str, generation: Option<i64>) -> Option<i64> {
    match prev {
        Some(p) if p.status == new_status => p.observed_generation,
        _ => generation,
    }
}

/// `containerStatuses[].lastState` (round 75) — the previous instance's
/// terminated details, if this codebase ever captured any (see
/// `CriRuntime`'s `last_terminated` side table / `TerminatedInfo`).
/// `Default::default()` (an empty `ContainerState`, matching upstream's
/// own "nothing to report yet" shape) when there's none.
/// `containerStatuses[].allocatedResourcesStatus` (round 79;
/// `ResourceHealthStatus`, KEP-4680) — groups `(resource_name, device_id,
/// health)` entries by resource name into the API's `ResourceStatus`
/// shape (one entry per resource, each with a list of per-device
/// `ResourceHealth`). `None` for an empty input, matching every other
/// "nothing to report" field on this struct — an entry never appears in
/// this list at all for a container with no device-plugin resources
/// allocated, rather than an empty list.
fn allocated_resources_status_field(entries: &[(String, String, String)]) -> Option<Vec<ResourceStatus>> {
    if entries.is_empty() {
        return None;
    }
    let mut by_resource: Vec<(String, Vec<ResourceHealth>)> = Vec::new();
    for (resource_name, device_id, health) in entries {
        let health_entry = ResourceHealth { resource_id: device_id.clone(), health: Some(health.clone()) };
        match by_resource.iter_mut().find(|(name, _)| name == resource_name) {
            Some((_, healths)) => healths.push(health_entry),
            None => by_resource.push((resource_name.clone(), vec![health_entry])),
        }
    }
    Some(by_resource.into_iter().map(|(name, resources)| ResourceStatus { name, resources: Some(resources) }).collect())
}

/// `containerStatuses[].user` (round 90; found in round 89's re-audit)
/// — `None` if this container's user was never fetched (mock runtime,
/// or the fetch failed), `Some` otherwise.
fn container_user_field(user: Option<&(i64, i64, Vec<i64>)>) -> Option<ContainerUser> {
    let &(uid, gid, ref supplemental_groups) = user?;
    Some(ContainerUser {
        linux: Some(LinuxContainerUser { uid, gid, supplemental_groups: (!supplemental_groups.is_empty()).then(|| supplemental_groups.clone()) }),
    })
}

/// `containerStatuses[].volumeMounts` (round 91; found in round 89's
/// re-audit) — `None` if the container has no `volumeMounts` at all,
/// `Some` (one entry per mount) otherwise.
fn volume_mount_statuses_field(mounts: &[(String, String, bool, Option<String>)]) -> Option<Vec<VolumeMountStatus>> {
    if mounts.is_empty() {
        return None;
    }
    Some(
        mounts
            .iter()
            .map(|(name, mount_path, read_only, recursive_read_only)| VolumeMountStatus {
                name: name.clone(),
                mount_path: mount_path.clone(),
                read_only: Some(*read_only),
                recursive_read_only: recursive_read_only.clone(),
            })
            .collect(),
    )
}

fn last_container_state(last: Option<&crate::runtime::TerminatedInfo>) -> ContainerState {
    match last {
        Some(info) => ContainerState {
            terminated: Some(ContainerStateTerminated {
                container_id: info.container_id.clone(),
                exit_code: info.exit_code,
                finished_at: info.finished_at.map(Time),
                message: (!info.message.is_empty()).then(|| info.message.clone()),
                reason: (!info.reason.is_empty()).then(|| info.reason.clone()),
                ..Default::default()
            }),
            ..Default::default()
        },
        None => ContainerState::default(),
    }
}

fn build_pod_status(
    host_ip: &str,
    ns: &str,
    name: &str,
    rt: &RuntimeStatus,
    prev: Option<&PodStatus>,
    readiness_gates: &[String],
    health: &HealthMap,
    qos: crate::eviction::QosClass,
    generation: Option<i64>,
) -> PodStatus {
    let running = rt.phase == Phase::Running;
    let prev_time = |type_: &str, status: &str| -> Option<Time> {
        prev?
            .conditions
            .as_ref()?
            .iter()
            .find(|c| c.type_ == type_ && c.status == status)
            .and_then(|c| c.last_transition_time.clone())
    };
    // `observedGeneration` (round 87; found in round 86's re-audit) —
    // by TYPE only (not type+status, unlike `prev_time` above), since
    // it needs to compare the new status against whatever this
    // condition's status *was*, not confirm it's unchanged first.
    let prev_condition = |type_: &str| -> Option<&PodCondition> { prev?.conditions.as_ref()?.iter().find(|c| c.type_ == type_) };
    let cond = |type_: &str, ok: bool| {
        let status = if ok { "True" } else { "False" }.to_string();
        let last_transition_time = prev_time(type_, &status)
            .or_else(|| Some(Time(k8s_openapi::jiff::Timestamp::now())));
        let observed_generation = condition_observed_generation(prev_condition(type_), &status, generation);
        PodCondition { type_: type_.to_string(), status, last_transition_time, observed_generation, ..Default::default() }
    };
    // Conditions an external controller set (e.g. to satisfy a
    // `readinessGates` entry) — must be carried forward into the new
    // `conditions` array nodelet writes below, or they'd be lost: the
    // apiserver patch is JSON Merge Patch (see `status_patch_changes()`'s
    // doc comment), which replaces the whole `conditions` array rather
    // than merging it element-by-element. Without this, a controller's
    // condition would vanish on nodelet's very next status write —
    // including the one `readinessGates` itself is trying to read.
    let foreign_conditions: Vec<PodCondition> = prev
        .and_then(|s| s.conditions.as_ref())
        .map(|conds| conds.iter().filter(|c| !OWNED_CONDITION_TYPES.contains(&c.type_.as_str())).cloned().collect())
        .unwrap_or_default();
    // A pod's aggregate `Ready` condition needs every `readinessGates`
    // entry's named condition to already be `True` too — checked against
    // `prev`'s conditions (the latest known state, same source
    // `foreign_conditions` reads from), not the ones being built here
    // (those are nodelet's own 4, never gate-relevant). A gate whose
    // condition doesn't exist yet at all counts as not-satisfied, same as
    // upstream.
    let gates_ready = readiness_gates.iter().all(|gate_type| {
        prev.and_then(|s| s.conditions.as_deref()).map(|conds| condition_is_true(conds, gate_type)).unwrap_or(false)
    });

    // A container is only actually Ready if it's running *and* its probe
    // supervisor (if any — probes::container_health defaults to healthy
    // when there's no supervisor tracking it) agrees: started (no startup
    // probe still pending) and passing its readiness probe.
    let container_ready = |c: &crate::runtime::ContainerRuntimeStatus| {
        if !c.running {
            return false;
        }
        let h = probes::container_health(health, ns, name, &c.name);
        h.started && h.ready
    };

    let container_statuses: Vec<ContainerStatus> = rt
        .containers
        .iter()
        .map(|c| ContainerStatus {
            name: c.name.clone(),
            image: c.image.clone(),
            image_id: c.image_id.clone(),
            ready: container_ready(c),
            restart_count: c.restart_count as i32,
            started: Some(c.running),
            container_id: c.container_id.clone(),
            state: Some(container_state(c, rt.started_at.map(Time), "ContainerCreating")),
            last_state: c.last_terminated.as_ref().map(|info| last_container_state(Some(info))),
            resources: c.resources.clone(),
            allocated_resources: c.allocated_resources.clone(),
            allocated_resources_status: allocated_resources_status_field(&c.allocated_resources_status),
            user: container_user_field(c.container_user.as_ref()),
            volume_mounts: volume_mount_statuses_field(&c.volume_mount_statuses),
            stop_signal: c.stop_signal.clone(),
            ..Default::default()
        })
        .collect();

    // In-place pod vertical scaling (round 43): a resize is still being
    // applied if any app container's actual last-applied resources
    // (`resources`) don't yet match what the current pod spec is asking
    // for (`allocatedResources`) — nodelet has no admission/deferral layer,
    // so there's no separate "pending" state to report, only "in
    // progress" vs. "done." `PodResizePending` is deliberately not
    // implemented — nothing in this codebase ever defers a resize.
    let resize_in_progress = rt.containers.iter().any(|c| match (&c.resources, &c.allocated_resources) {
        (Some(actual), Some(desired)) => actual.requests.clone().unwrap_or_default() != *desired,
        _ => false,
    });

    let init_container_statuses: Vec<ContainerStatus> = rt
        .init_containers
        .iter()
        .map(|c| ContainerStatus {
            name: c.name.clone(),
            image: c.image.clone(),
            image_id: c.image_id.clone(),
            // Native sidecars (round 36) get real probe-based readiness,
            // same as app containers — a regular (one-shot) init
            // container's readiness is just "is it currently running,"
            // matching upstream (it never has its own readinessProbe
            // honored, since it's already done and gone by the time
            // anything would check).
            ready: if c.is_restartable_sidecar { container_ready(c) } else { c.running },
            restart_count: c.restart_count as i32,
            started: Some(c.running),
            container_id: c.container_id.clone(),
            state: Some(container_state(c, None, "PodInitializing")),
            allocated_resources_status: allocated_resources_status_field(&c.allocated_resources_status),
            user: container_user_field(c.container_user.as_ref()),
            volume_mounts: volume_mount_statuses_field(&c.volume_mount_statuses),
            stop_signal: c.stop_signal.clone(),
            ..Default::default()
        })
        .collect();

    // A native sidecar's own readiness folds into the pod's overall
    // Ready/ContainersReady, same as an app container's does — it runs
    // for the pod's whole lifetime, so "is this pod fully up" has to
    // include it. A regular (already-exited) init container never
    // affects this, matching upstream.
    let all_ready = running
        && container_statuses.iter().all(|c| c.ready)
        && init_container_statuses.iter().zip(&rt.init_containers).all(|(status, c)| !c.is_restartable_sidecar || status.ready);

    // Ephemeral (debug) containers never gate readiness/phase — they're
    // reported for `kubectl describe`/`kubectl debug` visibility only, same
    // as init containers get their own status list rather than folding into
    // `container_statuses`.
    let ephemeral_container_statuses: Vec<ContainerStatus> = rt
        .ephemeral_containers
        .iter()
        .map(|c| ContainerStatus {
            name: c.name.clone(),
            image: c.image.clone(),
            image_id: c.image_id.clone(),
            ready: c.running,
            restart_count: c.restart_count as i32,
            started: Some(c.running),
            container_id: c.container_id.clone(),
            state: Some(if c.running {
                ContainerState { running: Some(ContainerStateRunning { started_at: None }), ..Default::default() }
            } else {
                ContainerState {
                    terminated: Some(ContainerStateTerminated { exit_code: 0, ..Default::default() }),
                    ..Default::default()
                }
            }),
            allocated_resources_status: allocated_resources_status_field(&c.allocated_resources_status),
            user: container_user_field(c.container_user.as_ref()),
            volume_mounts: volume_mount_statuses_field(&c.volume_mount_statuses),
            stop_signal: c.stop_signal.clone(),
            ..Default::default()
        })
        .collect();

    let pod_ips = rt
        .pod_ip
        .as_ref()
        .map(|ip| vec![PodIP { ip: ip.clone() }]);

    let mut conditions = vec![
        cond("Initialized", rt.initialized),
        cond("PodScheduled", true),
        cond("ContainersReady", all_ready),
        cond("Ready", all_ready && gates_ready),
        cond("PodResizeInProgress", resize_in_progress),
    ];
    conditions.extend(foreign_conditions);

    PodStatus {
        phase: Some(rt.phase.as_str().to_string()),
        conditions: Some(conditions),
        container_statuses: Some(container_statuses),
        init_container_statuses: (!init_container_statuses.is_empty()).then_some(init_container_statuses),
        ephemeral_container_statuses: (!ephemeral_container_statuses.is_empty()).then_some(ephemeral_container_statuses),
        host_ip: Some(host_ip.to_string()),
        // Round 56 (found in round 54's re-audit): real kubelet always
        // sets the plural `hostIPs` alongside the singular `hostIP`, even
        // on a single-stack node — mirrors the `podIP`/`podIPs` split
        // this file already gets right.
        host_ips: Some(vec![HostIP { ip: host_ip.to_string() }]),
        pod_ip: rt.pod_ip.clone(),
        pod_ips,
        start_time: rt.started_at.map(Time),
        message: rt.message.clone(),
        qos_class: Some(qos.as_str().to_string()),
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

/// Whether `status` is one of `ensure_pod()`'s "waiting on something
/// external that nothing else will retry" Pending cases — originally just
/// CSI volume attach (`pending_csi_volume_names()`, volumes_pure.rs),
/// round 124 also covers a missing device-plugin resource
/// (`pending_device_plugin_resources()`, resources.rs). Both are matched
/// by message prefix since `RuntimeStatus` has no dedicated reason enum
/// for this yet, same shortcut real kubelet's own event-message matching
/// in similar spots takes. Used by both `reconcile()` (decide whether to
/// schedule a retry at all) and `schedule_retry()` (decide whether to
/// keep chaining one) — deliberately named for the general shape ("some
/// external resource this pod needs isn't ready yet"), not just the CSI
/// case that motivated it first.
fn is_waiting_for_external_resource(status: &RuntimeStatus) -> bool {
    if status.phase != Phase::Pending {
        return false;
    }
    if status.message.as_deref().is_some_and(|m| {
        m.starts_with("waiting for CSI volume(s) to be mounted") || m.starts_with("waiting for device plugin resource(s) to be available")
    }) {
        return true;
    }
    // Round 124 (found live in CI): an ErrImagePull/ImagePullBackOff
    // container (see ensure_pod()'s post-build_status() synthesis in
    // pod_runtime_impl.rs) has no real CRI object and touches nothing on
    // the Pod object itself —
    // same "nothing else will ever retry this" shape as the CSI/device-
    // plugin cases above, so it needs the same explicit chain onto
    // schedule_retry() or a pull that starts succeeding again (once its
    // backoff window elapses) would never actually get retried.
    status.containers.iter().any(|c| matches!(c.waiting_reason_override.as_deref(), Some("ErrImagePull") | Some("ImagePullBackOff")))
}

fn key_parts(pod: &Pod) -> Option<(String, String)> {
    let ns = pod.metadata.namespace.clone().unwrap_or_else(|| "default".to_string());
    let name = pod.metadata.name.clone()?;
    Some((ns, name))
}

#[derive(Debug, Clone, Copy)]
enum ReferencedKind {
    ConfigMap,
    Secret,
}

/// Every ConfigMap name a pod's volumes reference — directly
/// (`volumes[].configMap.name`) or via a `projected` volume's
/// `sources[].configMap.name`. Used to decide which pods need
/// re-materializing when a ConfigMap changes. Deliberately does not look at
/// `envFrom`/`valueFrom.configMapKeyRef` — those are captured once at
/// container start and never refreshed, matching real kubelet.
fn referenced_configmap_names(pod: &Pod) -> HashSet<String> {
    let mut names = HashSet::new();
    for v in pod.spec.as_ref().and_then(|s| s.volumes.as_deref()).unwrap_or_default() {
        collect_configmap_name(v, &mut names);
    }
    names
}

fn collect_configmap_name(v: &Volume, names: &mut HashSet<String>) {
    if let Some(cm) = &v.config_map {
        names.insert(cm.name.clone());
    }
    if let Some(projected) = &v.projected {
        for source in projected.sources.as_deref().unwrap_or(&[]) {
            if let Some(cm) = &source.config_map {
                names.insert(cm.name.clone());
            }
        }
    }
}

/// Every Secret name a pod's volumes reference — directly
/// (`volumes[].secret.secretName`) or via a `projected` volume's
/// `sources[].secret.name`. Same env-var exclusion as
/// `referenced_configmap_names()` above.
fn referenced_secret_names(pod: &Pod) -> HashSet<String> {
    let mut names = HashSet::new();
    for v in pod.spec.as_ref().and_then(|s| s.volumes.as_deref()).unwrap_or_default() {
        collect_secret_name(v, &mut names);
    }
    names
}

fn collect_secret_name(v: &Volume, names: &mut HashSet<String>) {
    if let Some(sec) = &v.secret {
        if let Some(name) = &sec.secret_name {
            names.insert(name.clone());
        }
    }
    if let Some(projected) = &v.projected {
        for source in projected.sources.as_deref().unwrap_or(&[]) {
            if let Some(sec) = &source.secret {
                names.insert(sec.name.clone());
            }
        }
    }
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
#[cfg(test)]
#[path = "pods_tests/resize_status.rs"]
mod tests_resize_status;
#[cfg(test)]
#[path = "pods_tests/referenced_names.rs"]
mod tests_referenced_names;
#[cfg(test)]
#[path = "pods_tests/observed_generation.rs"]
mod tests_observed_generation;
#[cfg(test)]
#[path = "pods_tests/container_user.rs"]
mod tests_container_user;
