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
use k8s_openapi::api::core::v1::{Container, Probe};
use k8s_openapi::apimachinery::pkg::util::intstr::IntOrString;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
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
    failures: u32,
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

/// Spawn the probe supervisor for one pod. Returns immediately; the
/// returned handle should be aborted by the caller on pod teardown.
pub fn spawn(
    runtime: Arc<dyn PodRuntime>,
    health: HealthMap,
    namespace: String,
    name: String,
    containers: Vec<Container>,
    pod_ip: String,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut tasks = Vec::new();
        for container in containers {
            if !has_any_probe(std::slice::from_ref(&container)) {
                continue; // no probes on this container; default health is already "healthy"
            }
            let runtime = runtime.clone();
            let health = health.clone();
            let ns = namespace.clone();
            let name = name.clone();
            let pod_ip = pod_ip.clone();
            tasks.push(tokio::spawn(async move {
                probe_container(runtime, health, ns, name, container, pod_ip).await;
            }));
        }
        for t in tasks {
            let _ = t.await;
        }
    })
}

async fn probe_container(
    runtime: Arc<dyn PodRuntime>,
    health: HealthMap,
    ns: String,
    name: String,
    container: Container,
    pod_ip: String,
) {
    let cname = container.name.clone();
    let key = pod_key(&ns, &name);

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
            tokio::time::sleep(timing.period).await;
        }
        info!(pod = %key, container = %cname, "startup probe succeeded");
        set_health(&health, &ns, &name, &cname, |h| h.started = true);
    }

    let liveness = container.liveness_probe.clone().map(|p| (probe_timing(&p), probe_check(&p, &container)));
    let readiness = container.readiness_probe.clone().map(|p| (probe_timing(&p), probe_check(&p, &container)));
    if liveness.is_none() && readiness.is_none() {
        return;
    }

    let period = [liveness.as_ref().map(|(t, _)| t.period), readiness.as_ref().map(|(t, _)| t.period)]
        .into_iter()
        .flatten()
        .min()
        .unwrap_or(Duration::from_secs(10));
    let initial_delay = [liveness.as_ref().map(|(t, _)| t.initial_delay), readiness.as_ref().map(|(t, _)| t.initial_delay)]
        .into_iter()
        .flatten()
        .max()
        .unwrap_or_default();
    tokio::time::sleep(initial_delay).await;

    let mut live_tracker = liveness.as_ref().map(|(_, _)| ProbeTracker::new(true));
    let mut ready_tracker = readiness.as_ref().map(|(_, _)| ProbeTracker::new(false));

    loop {
        if let (Some((t, check)), Some(tracker)) = (&liveness, &mut live_tracker) {
            let ok = run_check(check, runtime.as_ref(), &ns, &name, &cname, &pod_ip, t.timeout).await;
            let was_passing = tracker.passing;
            tracker.record(ok, t.success_threshold, t.failure_threshold);
            if was_passing && !tracker.passing {
                warn!(pod = %key, container = %cname, "liveness probe failed; restarting container");
                if let Err(e) = runtime.restart_container(&ns, &name, &cname).await {
                    warn!(pod = %key, container = %cname, error = ?e, "restart_container failed");
                }
                *tracker = ProbeTracker::new(true); // give the fresh container a clean slate
            }
        }
        if let (Some((t, check)), Some(tracker)) = (&readiness, &mut ready_tracker) {
            let ok = run_check(check, runtime.as_ref(), &ns, &name, &cname, &pod_ip, t.timeout).await;
            tracker.record(ok, t.success_threshold, t.failure_threshold);
            set_health(&health, &ns, &name, &cname, |h| h.ready = tracker.passing);
        }
        tokio::time::sleep(period).await;
    }
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
