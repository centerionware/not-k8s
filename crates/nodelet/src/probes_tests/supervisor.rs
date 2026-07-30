//! Integration-style tests: the whole probe_container() loop driving a fake
//! runtime, proving readiness flips into HealthMap and a liveness failure
//! triggers restart_container — without a real CRI socket or containers.
//! Probe periods are clamped to a minimum of 1s (matches kubelet), so these
//! tests take a few real seconds each.

use super::*;
use crate::runtime::{PodRuntime, RuntimeStatus};
use async_trait::async_trait;
use k8s_openapi::api::core::v1::{Container, ExecAction, Pod, Probe};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc::UnboundedReceiver;

struct FakeRuntime {
    exec_ok: AtomicBool,
    restart_count: AtomicUsize,
}

impl FakeRuntime {
    fn new(initially_ok: bool) -> Arc<Self> {
        Arc::new(Self { exec_ok: AtomicBool::new(initially_ok), restart_count: AtomicUsize::new(0) })
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
    async fn restart_container(&self, _ns: &str, _name: &str, _container: &str) -> anyhow::Result<()> {
        self.restart_count.fetch_add(1, Ordering::SeqCst);
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
        health.clone(),
        "default".to_string(),
        "web".to_string(),
        container,
        "10.0.0.5".to_string(),
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
        health.clone(),
        "default".to_string(),
        "web".to_string(),
        container,
        "10.0.0.5".to_string(),
    ));

    tokio::time::sleep(Duration::from_millis(200)).await;
    runtime.exec_ok.store(false, Ordering::SeqCst); // start failing

    // failure_threshold=2, period=1s -> restart should fire a bit after 2s.
    tokio::time::sleep(Duration::from_millis(2600)).await;
    assert!(
        runtime.restart_count.load(Ordering::SeqCst) >= 1,
        "two consecutive liveness failures must trigger restart_container"
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
        health.clone(),
        "default".to_string(),
        "web".to_string(),
        container,
        "10.0.0.5".to_string(),
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
