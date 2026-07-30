//! In-memory mock runtime.
//!
//! Reports pods as Running without starting any real containers. On the first
//! `ensure_pod` it records the pod as `Pending`, then (after a short simulated
//! "startup" delay) flips it to `Running` and signals the controller via the
//! event channel — exercising the exact same event-driven status path the real
//! CRI runtime uses, but with zero container-engine overhead. Ideal for
//! measuring the control plane + agent idle cost in isolation.

use super::{ContainerRuntimeStatus, Phase, PodRuntime, RuntimeStatus};
use async_trait::async_trait;
use k8s_openapi::api::core::v1::Pod;
use k8s_openapi::jiff::Timestamp;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};

struct Inner {
    state: Mutex<HashMap<String, RuntimeStatus>>,
    tx: UnboundedSender<String>,
    rx: Mutex<Option<UnboundedReceiver<String>>>,
    /// Simulated container startup latency.
    startup: Duration,
}

#[derive(Clone)]
pub struct MockRuntime {
    inner: Arc<Inner>,
}

impl MockRuntime {
    pub fn new() -> Self {
        let (tx, rx) = unbounded_channel();
        Self {
            inner: Arc::new(Inner {
                state: Mutex::new(HashMap::new()),
                tx,
                rx: Mutex::new(Some(rx)),
                startup: Duration::from_millis(400),
            }),
        }
    }

    fn key(pod: &Pod) -> String {
        let ns = pod.metadata.namespace.as_deref().unwrap_or("default");
        let name = pod.metadata.name.as_deref().unwrap_or("");
        super::pod_key(ns, name)
    }
}

impl Default for MockRuntime {
    fn default() -> Self {
        Self::new()
    }
}

fn fake_pod_ip(key: &str) -> String {
    // Deterministic pseudo-IP in 10.42.0.0/16 from the key bytes.
    let h = key.bytes().fold(0u32, |a, b| a.wrapping_mul(31).wrapping_add(b as u32));
    format!("10.42.{}.{}", (h >> 8) & 0xff, (h & 0xfe) | 1)
}

fn containers_of(pod: &Pod, running: bool) -> Vec<ContainerRuntimeStatus> {
    pod.spec
        .as_ref()
        .map(|s| s.containers.as_slice())
        .unwrap_or(&[])
        .iter()
        .map(|c| ContainerRuntimeStatus {
            name: c.name.clone(),
            image: c.image.clone().unwrap_or_default(),
            ready: running,
            running,
            container_id: running.then(|| format!("mock://{}", c.name)),
            restart_count: 0,
        })
        .collect()
}

#[async_trait]
impl PodRuntime for MockRuntime {
    async fn ensure_pod(&self, pod: &Pod) -> anyhow::Result<RuntimeStatus> {
        let key = Self::key(pod);

        {
            let state = self.inner.state.lock().unwrap();
            if let Some(existing) = state.get(&key) {
                return Ok(existing.clone()); // idempotent
            }
        }

        // First sighting: record Pending, then schedule the flip to Running.
        let pending = RuntimeStatus {
            phase: Phase::Pending,
            message: Some("mock: creating sandbox".to_string()),
            started_at: None,
            pod_ip: None,
            containers: containers_of(pod, false),
            init_containers: Vec::new(),
            ephemeral_containers: Vec::new(),
            initialized: true,
        };
        self.inner.state.lock().unwrap().insert(key.clone(), pending.clone());

        let running = RuntimeStatus {
            phase: Phase::Running,
            message: None,
            started_at: Some(Timestamp::now()),
            pod_ip: Some(fake_pod_ip(&key)),
            containers: containers_of(pod, true),
            init_containers: Vec::new(),
            ephemeral_containers: Vec::new(),
            initialized: true,
        };

        let inner = self.inner.clone();
        let k = key.clone();
        tokio::spawn(async move {
            tokio::time::sleep(inner.startup).await;
            inner.state.lock().unwrap().insert(k.clone(), running);
            let _ = inner.tx.send(k); // event-driven status trigger
        });

        Ok(pending)
    }

    async fn remove_pod(&self, pod: &Pod) -> anyhow::Result<()> {
        let key = Self::key(pod);
        self.inner.state.lock().unwrap().remove(&key);
        Ok(())
    }

    async fn status(&self, namespace: &str, name: &str) -> anyhow::Result<Option<RuntimeStatus>> {
        let key = super::pod_key(namespace, name);
        Ok(self.inner.state.lock().unwrap().get(&key).cloned())
    }

    fn take_event_rx(&self) -> Option<UnboundedReceiver<String>> {
        self.inner.rx.lock().unwrap().take()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pod() -> Pod {
        serde_json::from_value(serde_json::json!({
            "apiVersion": "v1",
            "kind": "Pod",
            "metadata": {"name": "smoke-test", "namespace": "default"},
            "spec": {"containers": [{"name": "app", "image": "busybox"}]}
        }))
        .unwrap()
    }

    #[tokio::test]
    async fn repeated_ensure_does_not_schedule_duplicate_runtime_events() {
        let runtime = MockRuntime::new();
        let mut events = runtime.take_event_rx().expect("mock runtime event receiver");
        let pod = pod();

        runtime.ensure_pod(&pod).await.unwrap();
        runtime.ensure_pod(&pod).await.unwrap();

        let first = tokio::time::timeout(Duration::from_secs(1), events.recv())
            .await
            .expect("startup event should arrive")
            .expect("event channel should remain open");
        assert_eq!(first, "default/smoke-test");

        let second = tokio::time::timeout(Duration::from_millis(75), events.recv()).await;
        assert!(second.is_err(), "an unchanged ensure must not create a second runtime event");
    }
}
