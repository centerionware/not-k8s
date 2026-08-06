//! Integration-style tests: the whole probe_container() loop driving a fake
//! runtime, proving readiness flips into HealthMap and a liveness failure
//! triggers restart_container — without a real CRI socket or containers.
//! Probe periods are clamped to a minimum of 1s (matches kubelet), so these
//! tests take a few real seconds each.

use super::*;
use crate::runtime::{PodRuntime, RuntimeStatus};
use async_trait::async_trait;
use http::{Request, Response};
use k8s_openapi::api::core::v1::{Container, ExecAction, Pod, Probe};
use kube::client::Body;
use std::convert::Infallible;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc::UnboundedReceiver;
use tower::service_fn;

/// A `kube::Client` that 404s every request — enough for these tests:
/// `restart_and_reensure()`'s post-restart re-fetch sees "pod not found"
/// and skips calling `ensure_pod()` (which `FakeRuntime` deliberately
/// leaves `unimplemented!()`, since fully mocking real pod-ensure
/// behavior is out of scope for what these tests are actually proving —
/// that a probe failure calls `restart_container()` with the right
/// grace period). A real cluster's own re-fetch-and-reensure behavior is
/// out of this file's scope; see probes.rs's own `restart_and_reensure()`
/// doc comment for why it exists.
fn not_found_client() -> kube::Client {
    let service = service_fn(|_req: Request<Body>| async move {
        Ok::<_, Infallible>(Response::builder().status(404).body(Body::from(b"{}".to_vec())).unwrap())
    });
    kube::Client::new(service, "default")
}

struct FakeRuntime {
    exec_ok: AtomicBool,
    restart_count: AtomicUsize,
    last_grace_period_seconds: AtomicI64,
}

impl FakeRuntime {
    fn new(initially_ok: bool) -> Arc<Self> {
        Arc::new(Self {
            exec_ok: AtomicBool::new(initially_ok),
            restart_count: AtomicUsize::new(0),
            last_grace_period_seconds: AtomicI64::new(-1),
        })
    }
}

#[async_trait]
impl PodRuntime for FakeRuntime {
    async fn ensure_pod(&self, _pod: &Pod) -> anyhow::Result<RuntimeStatus> {
        unimplemented!("not exercised by these tests")
    }
    async fn remove_pod(&self, _pod: &Pod) -> anyhow::Result<()> {
        Ok(())
    }
    async fn status(&self, _namespace: &str, _name: &str) -> anyhow::Result<Option<RuntimeStatus>> {
        Ok(None)
    }
    fn take_event_rx(&self) -> Option<UnboundedReceiver<String>> {
        None
    }
    async fn exec(&self, _ns: &str, _name: &str, _container: &str, _command: &[String]) -> anyhow::Result<bool> {
        Ok(self.exec_ok.load(Ordering::SeqCst))
    }
    async fn restart_container(&self, _ns: &str, _name: &str, _container: &str, grace_period_seconds: i64) -> anyhow::Result<()> {
        self.restart_count.fetch_add(1, Ordering::SeqCst);
        self.last_grace_period_seconds.store(grace_period_seconds, Ordering::SeqCst);
        Ok(())
    }
}

fn exec_probe(command: &str, failure_threshold: i32) -> Probe {
    Probe {
        exec: Some(ExecAction { command: Some(vec![command.to_string()]) }),
        period_seconds: Some(1),
        failure_threshold: Some(failure_threshold),
        success_threshold: Some(1),
        ..Default::default()
    }
}

#[tokio::test]
async fn readiness_starts_false_and_flips_true_once_the_exec_probe_passes() {
    let runtime = FakeRuntime::new(false);
    let health = new_health_map();
    let container = Container {
        name: "app".to_string(),
        readiness_probe: Some(exec_probe("ready-check", 3)),
        ..Default::default()
    };

    let handle = tokio::spawn(probe_container(
        runtime.clone(),
        not_found_client(),
        health.clone(),
        "default".to_string(),
        "web".to_string(),
        container,
        "10.0.0.5".to_string(),
        30,
    ));

    // Give the loop a moment to seed "not ready" before the probe has run at all.
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(!container_health(&health, "default", "web", "app").ready, "must not be ready before any successful probe");

    runtime.exec_ok.store(true, Ordering::SeqCst);
    tokio::time::sleep(Duration::from_millis(1300)).await; // >= one 1s tick
    assert!(container_health(&health, "default", "web", "app").ready, "must become ready after the exec probe succeeds");

    handle.abort();
}

#[tokio::test]
async fn liveness_failure_past_threshold_triggers_a_container_restart() {
    let runtime = FakeRuntime::new(true); // starts healthy
    let health = new_health_map();
    let container = Container {
        name: "app".to_string(),
        liveness_probe: Some(exec_probe("live-check", 2)),
        ..Default::default()
    };

    let handle = tokio::spawn(probe_container(
        runtime.clone(),
        not_found_client(),
        health.clone(),
        "default".to_string(),
        "web".to_string(),
        container,
        "10.0.0.5".to_string(),
        30,
    ));

    tokio::time::sleep(Duration::from_millis(200)).await;
    runtime.exec_ok.store(false, Ordering::SeqCst); // start failing

    // failure_threshold=2, period=1s -> restart should fire a bit after 2s.
    tokio::time::sleep(Duration::from_millis(2600)).await;
    assert!(
        runtime.restart_count.load(Ordering::SeqCst) >= 1,
        "two consecutive liveness failures must trigger restart_container"
    );
    assert_eq!(
        runtime.last_grace_period_seconds.load(Ordering::SeqCst),
        30,
        "with no probe-level override, restart_container should get the pod's own grace period (30, passed in above)"
    );

    handle.abort();
}

#[tokio::test]
async fn liveness_probes_own_termination_grace_period_overrides_the_pods() {
    // Round 44: a liveness probe's own terminationGracePeriodSeconds wins
    // over the pod's — real kubelet's own documented override rule.
    let runtime = FakeRuntime::new(true);
    let health = new_health_map();
    let mut probe = exec_probe("live-check", 1);
    probe.termination_grace_period_seconds = Some(5);
    let container = Container { name: "app".to_string(), liveness_probe: Some(probe), ..Default::default() };

    let handle = tokio::spawn(probe_container(
        runtime.clone(),
        not_found_client(),
        health.clone(),
        "default".to_string(),
        "web".to_string(),
        container,
        "10.0.0.5".to_string(),
        30, // pod's own grace period — must be overridden by the probe's 5
    ));

    tokio::time::sleep(Duration::from_millis(200)).await;
    runtime.exec_ok.store(false, Ordering::SeqCst);
    tokio::time::sleep(Duration::from_millis(1600)).await;

    assert!(runtime.restart_count.load(Ordering::SeqCst) >= 1, "liveness failure must trigger a restart");
    assert_eq!(
        runtime.last_grace_period_seconds.load(Ordering::SeqCst),
        5,
        "the probe's own terminationGracePeriodSeconds (5) must win over the pod's (30)"
    );

    handle.abort();
}

#[tokio::test]
async fn startup_probe_gates_readiness_until_it_passes() {
    let runtime = FakeRuntime::new(false); // startup probe fails at first
    let health = new_health_map();
    let container = Container {
        name: "app".to_string(),
        startup_probe: Some(exec_probe("boot-check", 1)),
        readiness_probe: Some(exec_probe("ready-check", 1)),
        ..Default::default()
    };

    let handle = tokio::spawn(probe_container(
        runtime.clone(),
        not_found_client(),
        health.clone(),
        "default".to_string(),
        "web".to_string(),
        container,
        "10.0.0.5".to_string(),
        30,
    ));

    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(!container_health(&health, "default", "web", "app").started, "startup probe hasn't passed yet");
    assert!(!container_health(&health, "default", "web", "app").ready, "readiness must not even start probing yet");

    runtime.exec_ok.store(true, Ordering::SeqCst);
    tokio::time::sleep(Duration::from_millis(2300)).await; // startup tick + readiness tick
    assert!(container_health(&health, "default", "web", "app").started, "startup probe should have passed by now");
    assert!(container_health(&health, "default", "web", "app").ready, "readiness should follow once started");

    handle.abort();
}

#[tokio::test]
async fn spawn_returns_one_independently_abortable_handle_per_probed_container() {
    // Regression test for the restart-storm bug: spawn() used to wrap all
    // per-container loops in one outer task and return only that outer
    // handle, so stop_probe_supervisor()'s single abort() never reached the
    // inner loops — they kept running (and kept calling restart_container())
    // forever after the pod's probe supervisor was supposedly stopped.
    // Confirmed live at 810 restart attempts in 90s for one two-container
    // pod. spawn() must instead return one handle per container, every one
    // of which the caller can abort individually.
    let runtime = FakeRuntime::new(true);
    let health = new_health_map();
    let containers = vec![
        Container { name: "app".to_string(), liveness_probe: Some(exec_probe("live-check", 1)), ..Default::default() },
        Container { name: "sidecar".to_string(), liveness_probe: Some(exec_probe("live-check", 1)), ..Default::default() },
        Container { name: "no-probe".to_string(), ..Default::default() }, // must not get a task at all
    ];

    let handles = super::spawn(runtime.clone(), not_found_client(), health.clone(), "default".to_string(), "web".to_string(), containers, "10.0.0.5".to_string(), 30);
    assert_eq!(handles.len(), 2, "one task per probed container, none for the container with no probes");

    // Abort every returned handle, as stop_probe_supervisor() does.
    for handle in &handles {
        handle.abort();
    }
    for handle in handles {
        let _ = handle.await; // aborted tasks resolve Err(JoinError), not hang
    }

    // Once aborted, no further restarts should happen no matter how long we wait.
    runtime.exec_ok.store(false, Ordering::SeqCst);
    let count_after_abort = runtime.restart_count.load(Ordering::SeqCst);
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(
        runtime.restart_count.load(Ordering::SeqCst),
        count_after_abort,
        "aborted probe tasks must not still be running and calling restart_container()"
    );
}

#[tokio::test]
async fn startup_probe_failure_past_threshold_triggers_a_restart_and_recovers() {
    // Round 47: real kubelet kills and restarts the container once a
    // startup probe fails past its own failureThreshold, exactly like a
    // liveness failure — previously this loop retried forever with no
    // restart at all.
    let runtime = FakeRuntime::new(false); // keeps failing
    let health = new_health_map();
    let container = Container {
        name: "app".to_string(),
        startup_probe: Some(exec_probe("boot-check", 2)),
        ..Default::default()
    };

    let handle = tokio::spawn(probe_container(
        runtime.clone(),
        not_found_client(),
        health.clone(),
        "default".to_string(),
        "web".to_string(),
        container,
        "10.0.0.5".to_string(),
        30,
    ));

    // failure_threshold=2, period=1s -> restart fires around t=2s.
    tokio::time::sleep(Duration::from_millis(2600)).await;
    assert!(
        runtime.restart_count.load(Ordering::SeqCst) >= 1,
        "a startup probe failing past its failureThreshold must trigger a restart"
    );
    assert_eq!(
        runtime.last_grace_period_seconds.load(Ordering::SeqCst),
        30,
        "restart_container should get the pod's own grace period, same as a liveness-triggered restart"
    );
    assert!(!container_health(&health, "default", "web", "app").started, "still not started — it keeps failing");

    // The supervisor task must keep retrying after the restart (same task,
    // whole container lifetime), not give up — let it start passing now.
    runtime.exec_ok.store(true, Ordering::SeqCst);
    tokio::time::sleep(Duration::from_millis(2300)).await;
    assert!(container_health(&health, "default", "web", "app").started, "should recover and pass once the (recreated) container starts succeeding");

    handle.abort();
}
