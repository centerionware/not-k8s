//! Liveness/readiness/startup probe execution.
//!
//! Kept strictly opt-in: `PodController` only spawns a supervisor for a pod
//! that actually declares at least one probe (see `has_any_probe`). A node
//! running zero probed pods adds zero periodic work — the idle-cost story
//! the rest of nodelet is built around (`runtime/mod.rs`) is unaffected.
//!
//! Each probed container gets one supervisor task with up to two phases:
//!   1. startup gating, if a `startupProbe` is set — liveness/readiness
//!      don't run until it passes (kubelet semantics), and
//!   2. a combined liveness+readiness loop, ticking at the faster of the
//!      two configured periods (a simplification vs. kubelet's fully
//!      independent per-probe timers — acceptable for single-node edge use;
//!      see docs/GAP_CLOSURE.md).
//!
//! A liveness failure calls `PodRuntime::restart_container` directly; a
//! readiness result is written into the shared `HealthMap`, which
//! `pods.rs::build_pod_status` reads when computing per-container `ready`
//! and the pod-level `Ready`/`ContainersReady` conditions.

use crate::runtime::{pod_key, PodRuntime};
use k8s_openapi::api::core::v1::{Container, Pod, Probe};
use k8s_openapi::apimachinery::pkg::util::intstr::IntOrString;
use kube::api::Api;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc::UnboundedSender;
use tracing::{info, warn};

/// Per-container live health, keyed by `namespace/name` then container name.
pub type HealthMap = Arc<Mutex<HashMap<String, HashMap<String, ContainerHealth>>>>;

#[derive(Clone, Copy, Debug)]
pub struct ContainerHealth {
    /// Whether this container currently passes its readiness probe (or has
    /// no readiness probe, in which case it's `true` once seeded).
    pub ready: bool,
    /// Whether its startup probe (if any) has passed yet. Gates whether
    /// `ready` should be trusted at all — a container still starting up is
    /// never Ready regardless of `ready`'s value.
    pub started: bool,
}

impl Default for ContainerHealth {
    fn default() -> Self {
        Self { ready: true, started: true }
    }
}

pub fn new_health_map() -> HealthMap {
    Arc::new(Mutex::new(HashMap::new()))
}

/// Read the current health for a container, defaulting to "healthy" (ready +
/// started) when there's no supervisor tracking it yet — matches kubelet's
/// "no probes configured -> Ready as soon as Running" default.
pub fn container_health(health: &HealthMap, ns: &str, name: &str, container: &str) -> ContainerHealth {
    health
        .lock()
        .unwrap()
        .get(&pod_key(ns, name))
        .and_then(|m| m.get(container))
        .copied()
        .unwrap_or_default()
}

pub fn clear_pod_health(health: &HealthMap, ns: &str, name: &str) {
    health.lock().unwrap().remove(&pod_key(ns, name));
}

/// Seed probe health synchronously, before the supervisor task is spawned.
///
/// `PodController` writes the first Pod status immediately after spawning the
/// supervisor. If the task has not been polled yet, the health map's default
/// says that every container is ready, allowing an OrderedReady StatefulSet
/// to create the next ordinal before its readiness probe has run once.
pub fn initialize_health(health: &HealthMap, ns: &str, name: &str, containers: &[Container]) {
    for container in containers {
        if container.readiness_probe.is_none() && container.startup_probe.is_none() {
            continue;
        }
        let cname = container.name.clone();
        let has_readiness = container.readiness_probe.is_some();
        let has_startup = container.startup_probe.is_some();
        set_health(health, ns, name, &cname, |entry| {
            if has_readiness || has_startup {
                entry.ready = false;
            }
            if has_startup {
                entry.started = false;
            }
        });
    }
}

fn set_health(health: &HealthMap, ns: &str, name: &str, container: &str, f: impl FnOnce(&mut ContainerHealth)) {
    let mut map = health.lock().unwrap();
    let entry = map
        .entry(pod_key(ns, name))
        .or_default()
        .entry(container.to_string())
        .or_default();
    f(entry);
}

pub fn has_any_probe(containers: &[Container]) -> bool {
    containers
        .iter()
        .any(|c| c.liveness_probe.is_some() || c.readiness_probe.is_some() || c.startup_probe.is_some())
}

/// What kind of check a `Probe` describes, with everything resolved to
/// concrete values (port numbers, command). `None` for the rare case none of
/// `httpGet`/`tcpSocket`/`exec` is actually set (schema allows it).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProbeCheck {
    Http { path: String, port: u16, https: bool },
    Tcp { port: u16 },
    Exec { command: Vec<String> },
    /// `probe.grpc` (round 29) — the standard `grpc.health.v1.Health/Check`
    /// protocol. `service` is the (optional) service name to place in the
    /// `HealthCheckRequest`; unset means "overall server health," matching
    /// the field's own documented default.
    Grpc { port: u16, service: Option<String> },
    None,
}

/// Resolve a probe's port against the container's declared ports (a
/// `containerPort` name, e.g. `http`) or a bare numeric port.
fn resolve_port(port: &IntOrString, container: &Container) -> u16 {
    match port {
        IntOrString::Int(n) => *n as u16,
        IntOrString::String(name) => container
            .ports
            .as_deref()
            .unwrap_or(&[])
            .iter()
            .find(|p| p.name.as_deref() == Some(name.as_str()))
            .map(|p| p.container_port as u16)
            .unwrap_or(0),
    }
}

pub fn probe_check(probe: &Probe, container: &Container) -> ProbeCheck {
    if let Some(http) = &probe.http_get {
        let port = resolve_port(&http.port, container);
        let path = http.path.clone().unwrap_or_else(|| "/".to_string());
        let https = http.scheme.as_deref() == Some("HTTPS");
        ProbeCheck::Http { path, port, https }
    } else if let Some(tcp) = &probe.tcp_socket {
        ProbeCheck::Tcp { port: resolve_port(&tcp.port, container) }
    } else if let Some(exec) = &probe.exec {
        ProbeCheck::Exec { command: exec.command.clone().unwrap_or_default() }
    } else if let Some(grpc) = &probe.grpc {
        ProbeCheck::Grpc { port: grpc.port as u16, service: grpc.service.clone() }
    } else {
        ProbeCheck::None
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProbeTiming {
    pub initial_delay: Duration,
    pub period: Duration,
    pub timeout: Duration,
    pub success_threshold: u32,
    pub failure_threshold: u32,
}

pub fn probe_timing(probe: &Probe) -> ProbeTiming {
    ProbeTiming {
        initial_delay: Duration::from_secs(probe.initial_delay_seconds.unwrap_or(0).max(0) as u64),
        period: Duration::from_secs(probe.period_seconds.unwrap_or(10).max(1) as u64),
        timeout: Duration::from_secs(probe.timeout_seconds.unwrap_or(1).max(1) as u64),
        success_threshold: probe.success_threshold.unwrap_or(1).max(1) as u32,
        failure_threshold: probe.failure_threshold.unwrap_or(3).max(1) as u32,
    }
}

/// Pure success/failure-threshold state machine — kubelet semantics: the
/// passing/failing verdict only flips once `threshold` *consecutive*
/// matching outcomes have been seen; an isolated opposite outcome resets the
/// other counter but doesn't flip anything by itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProbeTracker {
    successes: u32,
    pub failures: u32,
    pub passing: bool,
}

impl ProbeTracker {
    pub fn new(initially_passing: bool) -> Self {
        Self { successes: 0, failures: 0, passing: initially_passing }
    }

    pub fn record(&mut self, success: bool, success_threshold: u32, failure_threshold: u32) {
        let success_threshold = success_threshold.max(1);
        let failure_threshold = failure_threshold.max(1);
        if success {
            self.failures = 0;
            self.successes += 1;
            if self.successes >= success_threshold {
                self.passing = true;
            }
        } else {
            self.successes = 0;
            self.failures += 1;
            if self.failures >= failure_threshold {
                self.passing = false;
            }
        }
    }
}

/// Parse an HTTP/1.x response's status line and report whether it's in the
/// success range kubelet uses for httpGet probes (`200 <= code < 400`).
fn parse_http_status_ok(buf: &[u8]) -> Option<bool> {
    let text = std::str::from_utf8(buf).ok()?;
    let line = text.lines().next()?;
    let mut parts = line.split_whitespace();
    let _version = parts.next().filter(|v| v.starts_with("HTTP/"))?;
    let code: u16 = parts.next()?.parse().ok()?;
    Some((200..400).contains(&code))
}

async fn check_tcp(host: &str, port: u16, timeout: Duration) -> bool {
    if port == 0 {
        return false;
    }
    matches!(tokio::time::timeout(timeout, TcpStream::connect((host, port))).await, Ok(Ok(_)))
}

async fn check_http(host: &str, port: u16, path: &str, timeout: Duration) -> bool {
    if port == 0 {
        return false;
    }
    let host = host.to_string();
    let path = path.to_string();
    tokio::time::timeout(timeout, async move {
        let mut stream = TcpStream::connect((host.as_str(), port)).await.ok()?;
        let req = format!("GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n");
        stream.write_all(req.as_bytes()).await.ok()?;
        let mut buf = Vec::with_capacity(256);
        let mut chunk = [0u8; 512];
        loop {
            let n = stream.read(&mut chunk).await.ok()?;
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&chunk[..n]);
            if buf.contains(&b'\n') {
                break; // status line is complete; that's all parse_http_status_ok needs
            }
        }
        parse_http_status_ok(&buf)
    })
    .await
    .ok()
    .flatten()
    .unwrap_or(false)
}

/// Generated `grpc.health.v1` types/client (from `proto/health.proto`) —
/// `cri`-only since it needs `tonic`'s transport, same as CSI/device
/// plugin clients elsewhere in this codebase. Not compiled in a mock-only
/// build; `check_grpc()` below has a matching stub for that case.
#[cfg(feature = "cri")]
mod grpc_health {
    tonic::include_proto!("grpc.health.v1");
}

#[cfg(feature = "cri")]
async fn check_grpc(host: &str, port: u16, service: Option<&str>, timeout: Duration) -> bool {
    use grpc_health::health_client::HealthClient;
    use grpc_health::{health_check_response::ServingStatus, HealthCheckRequest};

    if port == 0 {
        return false;
    }
    let host = host.to_string();
    let service = service.unwrap_or_default().to_string();
    tokio::time::timeout(timeout, async move {
        let endpoint = tonic::transport::Endpoint::try_from(format!("http://{host}:{port}")).ok()?;
        let channel = endpoint.connect().await.ok()?;
        let mut client = HealthClient::new(channel);
        let resp = client.check(HealthCheckRequest { service }).await.ok()?;
        Some(resp.into_inner().status == ServingStatus::Serving as i32)
    })
    .await
    .ok()
    .flatten()
    .unwrap_or(false)
}

/// No `tonic` transport in a mock-only build (see `grpc_health` above) —
/// a `grpc` probe simply can't be checked here, same treatment `exec`
/// probes would get without a real runtime to exec into. Always `false`
/// (not-ready/not-alive) rather than silently claiming success.
#[cfg(not(feature = "cri"))]
async fn check_grpc(_host: &str, _port: u16, _service: Option<&str>, _timeout: Duration) -> bool {
    false
}

async fn run_check(
    check: &ProbeCheck,
    runtime: &dyn PodRuntime,
    ns: &str,
    name: &str,
    container: &str,
    pod_ip: &str,
    timeout: Duration,
) -> bool {
    match check {
        ProbeCheck::Http { path, port, https } => {
            if *https {
                // TLS probing isn't implemented; a bare connect at least
                // proves the port accepts connections. See docs/GAP_CLOSURE.md.
                check_tcp(pod_ip, *port, timeout).await
            } else {
                check_http(pod_ip, *port, path, timeout).await
            }
        }
        ProbeCheck::Tcp { port } => check_tcp(pod_ip, *port, timeout).await,
        ProbeCheck::Grpc { port, service } => check_grpc(pod_ip, *port, service.as_deref(), timeout).await,
        ProbeCheck::Exec { command } => {
            tokio::time::timeout(timeout, runtime.exec(ns, name, container, command))
                .await
                .ok()
                .and_then(|r| r.ok())
                .unwrap_or(false)
        }
        ProbeCheck::None => true,
    }
}

/// Spawn the probe supervisor for one pod: one task per container that
/// declares a probe. Returns immediately; every returned handle should be
/// aborted by the caller on pod teardown — all of them, not just one.
///
/// Deliberately *not* one outer task that itself spawns these and returns
/// only its own handle: `JoinHandle::abort()` cancels exactly the task it
/// names, never anything that task went on to `tokio::spawn()` — those are
/// independent, detached tasks the runtime doesn't treat as children of
/// whoever spawned them. Confirmed for real, live: with a single
/// wrapping-task handle (this function's previous shape), the per-container
/// loops it spawned kept running forever after `stop_probe_supervisor()`'s
/// abort() — every container restart leaked the old probe loops instead of
/// stopping them, and each orphaned loop kept independently polling and
/// calling `restart_container()` on its own schedule. Accumulate enough of
/// those (one per restart) and it snowballs into a genuine restart storm:
/// confirmed at 810 restart attempts in 90s for one two-container pod,
/// installing a real CSI driver whose containers happened to restart a
/// few times early on before stabilizing.
pub fn spawn(
    runtime: Arc<dyn PodRuntime>,
    client: kube::Client,
    health: HealthMap,
    notify: UnboundedSender<String>,
    namespace: String,
    name: String,
    containers: Vec<Container>,
    pod_ip: String,
    pod_grace_period_seconds: i64,
) -> Vec<tokio::task::JoinHandle<()>> {
    let mut tasks = Vec::new();
    for container in containers {
        if !has_any_probe(std::slice::from_ref(&container)) {
            continue; // no probes on this container; default health is already "healthy"
        }
        let runtime = runtime.clone();
        let client = client.clone();
        let health = health.clone();
        let notify = notify.clone();
        let ns = namespace.clone();
        let name = name.clone();
        let pod_ip = pod_ip.clone();
        tasks.push(tokio::spawn(async move {
            probe_container(runtime, client, health, notify, ns, name, container, pod_ip, pod_grace_period_seconds).await;
        }));
    }
    tasks
}

/// A probe's own `terminationGracePeriodSeconds` override (only meaningful
/// on liveness/startup probes — readiness failures never kill a container)
/// if set, else the pod's own (round 44; found in round 35's re-audit).
/// Matches real kubelet's documented default: "If this value is nil, the
/// pod's terminationGracePeriodSeconds will be used."
fn probe_grace_period_seconds(probe: &Probe, pod_grace_period_seconds: i64) -> i64 {
    probe.termination_grace_period_seconds.filter(|s| *s >= 0).unwrap_or(pod_grace_period_seconds)
}

/// `runtime.restart_container()` only kills and removes the old container
/// — it doesn't (can't, from inside the `PodRuntime` trait impl alone)
/// recreate one, since that needs the pod's full current spec (volumes,
/// pull secrets, resolved claim devices, ...) which only `ensure_pod()`
/// knows how to gather. Real kubelet's own SyncPod treats a probe-
/// triggered restart as kill-then-immediately-start, one atomic sync —
/// this does the same by re-fetching the live Pod and calling
/// `ensure_pod()` again right after the removal, rather than leaving the
/// container missing until *something else* happens to trigger a
/// reconcile for this pod (a watch event on the Pod object itself, which
/// a purely-internal probe-driven restart never generates).
///
/// Found live (not hypothetical): round 123's first full CI e2e run hit
/// this exactly — coredns's liveness probe failed once, restart_container()
/// removed its container, and with no unrelated event ever touching
/// coredns's own Pod object again for the rest of the run, nothing ever
/// recreated it — every subsequent probe cycle found no container,
/// "restarted" (a no-op — already gone) forever, and the whole node's
/// DNS (and everything downstream of it) stayed broken for the rest of
/// the run.
async fn restart_and_reensure(runtime: &Arc<dyn PodRuntime>, client: &kube::Client, ns: &str, name: &str, cname: &str, grace: i64) {
    if let Err(e) = runtime.restart_container(ns, name, cname, grace).await {
        warn!(pod = %pod_key(ns, name), container = %cname, error = ?e, "restart_container failed");
        return;
    }
    let api: Api<Pod> = Api::namespaced(client.clone(), ns);
    match api.get_opt(name).await {
        Ok(Some(pod)) if pod.metadata.deletion_timestamp.is_none() => {
            if let Err(e) = runtime.ensure_pod(&pod).await {
                warn!(pod = %pod_key(ns, name), container = %cname, error = ?e, "re-ensure after probe-triggered restart failed; will retry next probe cycle");
            }
        }
        Ok(_) => {} // pod gone or being deleted — nothing to recreate
        Err(e) => warn!(pod = %pod_key(ns, name), container = %cname, error = ?e, "failed to re-fetch pod after probe-triggered restart"),
    }
}

async fn probe_container(
    runtime: Arc<dyn PodRuntime>,
    client: kube::Client,
    health: HealthMap,
    notify: UnboundedSender<String>,
    ns: String,
    name: String,
    container: Container,
    pod_ip: String,
    pod_grace_period_seconds: i64,
) {
    let cname = container.name.clone();
    let key = pod_key(&ns, &name);
    // Round 123: a readiness/started transition changes real pod status
    // (conditions[Ready], containerStatuses[].ready/started) but never
    // touches the apiserver or the CRI event stream on its own — nothing
    // else would notice. A closed receiver (PodController shutting down)
    // just means these sends become no-ops; never worth failing a probe
    // loop over.
    let notify_reconcile = || { let _ = notify.send(key.clone()); };

    if container.readiness_probe.is_some() {
        set_health(&health, &ns, &name, &cname, |h| h.ready = false);
    }

    if let Some(startup) = container.startup_probe.clone() {
        set_health(&health, &ns, &name, &cname, |h| h.started = false);
        let timing = probe_timing(&startup);
        let check = probe_check(&startup, &container);
        tokio::time::sleep(timing.initial_delay).await;
        let mut tracker = ProbeTracker::new(false);
        loop {
            let ok = run_check(&check, runtime.as_ref(), &ns, &name, &cname, &pod_ip, timing.timeout).await;
            tracker.record(ok, timing.success_threshold, timing.failure_threshold);
            if tracker.passing {
                break;
            }
            // Round 47 (found in round 45's re-audit): real kubelet kills
            // and restarts the container once a startup probe fails past
            // its own failureThreshold, exactly like a liveness failure —
            // previously this loop retried forever with no restart at all.
            // This same supervisor task is spawned once per container for
            // its whole lifetime (`ensure_probe_supervisor()`), so after a
            // restart-triggering failure the loop keeps going (doesn't
            // return) and re-attempts startup probing fresh for the new
            // container instance, rather than ending the supervisor task.
            if tracker.failures >= timing.failure_threshold.max(1) {
                let grace = probe_grace_period_seconds(&startup, pod_grace_period_seconds);
                warn!(pod = %key, container = %cname, grace_period_seconds = grace, "startup probe failed; restarting container");
                restart_and_reensure(&runtime, &client, &ns, &name, &cname, grace).await;
                tracker = ProbeTracker::new(false);
            }
            tokio::time::sleep(timing.period).await;
        }
        info!(pod = %key, container = %cname, "startup probe succeeded");
        set_health(&health, &ns, &name, &cname, |h| h.started = true);
        notify_reconcile();
    }

    let liveness = container.liveness_probe.clone().map(|p| (probe_timing(&p), probe_check(&p, &container)));
    let readiness = container.readiness_probe.clone().map(|p| (probe_timing(&p), probe_check(&p, &container)));
    if liveness.is_none() && readiness.is_none() {
        return;
    }

    // Each probe owns its delay, period and in-flight check. In particular,
    // CoreDNS's 60s liveness grace must not postpone its readiness check.
    // Join within this container's task so aborting it cancels both futures;
    // neither becomes a detached task that survives Pod deletion.
    let live_loop = async {
        let Some((t, check)) = &liveness else { return };
        tokio::time::sleep(t.initial_delay).await;
        let mut tracker = ProbeTracker::new(true);
        loop {
            let ok = run_check(check, runtime.as_ref(), &ns, &name, &cname, &pod_ip, t.timeout).await;
            let was_passing = tracker.passing;
            tracker.record(ok, t.success_threshold, t.failure_threshold);
            if was_passing && !tracker.passing {
                let grace = probe_grace_period_seconds(container.liveness_probe.as_ref().unwrap(), pod_grace_period_seconds);
                warn!(pod = %key, container = %cname, grace_period_seconds = grace, "liveness probe failed; restarting container");
                restart_and_reensure(&runtime, &client, &ns, &name, &cname, grace).await;
                tracker = ProbeTracker::new(true); // give the fresh container a clean slate
            }
            tokio::time::sleep(t.period).await;
        }
    };
    let ready_loop = async {
        let Some((t, check)) = &readiness else { return };
        tokio::time::sleep(t.initial_delay).await;
        let mut tracker = ProbeTracker::new(false);
        loop {
            let ok = run_check(check, runtime.as_ref(), &ns, &name, &cname, &pod_ip, t.timeout).await;
            let was_passing = tracker.passing;
            tracker.record(ok, t.success_threshold, t.failure_threshold);
            set_health(&health, &ns, &name, &cname, |h| h.ready = tracker.passing);
            if tracker.passing != was_passing {
                notify_reconcile();
            }
            tokio::time::sleep(t.period).await;
        }
    };
    tokio::join!(live_loop, ready_loop);
}

#[cfg(test)]
#[path = "probes_tests/tracker.rs"]
mod tests_tracker;
#[cfg(test)]
#[path = "probes_tests/check_extraction.rs"]
mod tests_check_extraction;
#[cfg(test)]
#[path = "probes_tests/http_parse.rs"]
mod tests_http_parse;
#[cfg(test)]
#[path = "probes_tests/network_checks.rs"]
mod tests_network_checks;
#[cfg(test)]
#[path = "probes_tests/supervisor.rs"]
mod tests_supervisor;
#[cfg(test)]
#[path = "probes_tests/grace_period.rs"]
mod tests_grace_period;
