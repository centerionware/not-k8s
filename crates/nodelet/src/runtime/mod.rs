//! Pluggable pod runtime.
//!
//! The controller is runtime-agnostic: it reconciles desired Pods (from the
//! apiserver) against whatever `PodRuntime` is plugged in. Two implementations:
//!
//! * [`mock`] — in-memory, no container engine; reports pods Running instantly.
//!   Used to exercise and measure the control loop with ~zero overhead.
//! * `cri` — real containerd via the CRI gRPC API (compiled with `--features cri`).
//!
//! Crucially, the runtime is *event-driven*: instead of the controller polling
//! every container every second (the kubelet's PLEG), the runtime pushes a key
//! onto an mpsc channel whenever a pod's state changes, and the controller
//! reconciles just that pod. That is the core idle-CPU win.

pub mod mock;

#[cfg(feature = "cri")]
pub mod cri;

use async_trait::async_trait;
use k8s_openapi::api::core::v1::Pod;
use k8s_openapi::jiff::Timestamp;
use tokio::sync::mpsc::UnboundedReceiver;

/// High-level pod phase, mirroring `PodStatus.phase`.
/// `Succeeded`/`Failed` are produced by the CRI runtime (feature-gated).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(dead_code)] // Succeeded/Failed are only produced by the CRI runtime
pub enum Phase {
    Pending,
    Running,
    Succeeded,
    Failed,
}

impl Phase {
    pub fn as_str(self) -> &'static str {
        match self {
            Phase::Pending => "Pending",
            Phase::Running => "Running",
            Phase::Succeeded => "Succeeded",
            Phase::Failed => "Failed",
        }
    }
}

#[derive(Clone, Debug)]
pub struct ContainerRuntimeStatus {
    pub name: String,
    pub image: String,
    pub ready: bool,
    pub running: bool,
    pub container_id: Option<String>,
    /// Cumulative restart count, matching `PodStatus.containerStatuses[].restartCount`.
    pub restart_count: u32,
}

#[derive(Clone, Debug)]
pub struct RuntimeStatus {
    pub phase: Phase,
    pub message: Option<String>,
    pub started_at: Option<Timestamp>,
    pub pod_ip: Option<String>,
    pub containers: Vec<ContainerRuntimeStatus>,
}

/// `namespace/name` key identifying a pod.
pub fn pod_key(namespace: &str, name: &str) -> String {
    format!("{namespace}/{name}")
}

#[async_trait]
pub trait PodRuntime: Send + Sync {
    /// Ensure the pod's containers are created and started. Idempotent.
    async fn ensure_pod(&self, pod: &Pod) -> anyhow::Result<RuntimeStatus>;

    /// Tear down the pod's sandbox and containers. Idempotent. Takes the
    /// full Pod (not just its key) because a graceful teardown needs
    /// `terminationGracePeriodSeconds` and each container's `preStop` hook.
    async fn remove_pod(&self, pod: &Pod) -> anyhow::Result<()>;

    /// Current runtime status, or `None` if the pod is unknown to the runtime.
    async fn status(&self, namespace: &str, name: &str) -> anyhow::Result<Option<RuntimeStatus>>;

    /// Take the event channel that signals "this pod's runtime state changed".
    /// Called once at startup; the controller owns the receiver thereafter.
    /// Returning `None` means the runtime produces no async events.
    fn take_event_rx(&self) -> Option<UnboundedReceiver<String>>;

    /// Run `command` inside the named pod's named container and report
    /// whether it exited zero (exec-probe semantics). Default: always
    /// succeeds — runtimes with nothing real to exec into (mock) have no
    /// way to fail, so an exec probe against one is a no-op pass rather
    /// than a hard error.
    async fn exec(&self, namespace: &str, name: &str, container: &str, command: &[String]) -> anyhow::Result<bool> {
        let _ = (namespace, name, container, command);
        Ok(true)
    }

    /// Kill just this one container (liveness-probe-triggered restart). The
    /// next `ensure_pod()` call recreates it via the runtime's normal
    /// "missing container -> create fresh" path, so this only needs to make
    /// the old one gone. Default: no-op — nothing real to restart (mock).
    async fn restart_container(&self, namespace: &str, name: &str, container: &str) -> anyhow::Result<()> {
        let _ = (namespace, name, container);
        Ok(())
    }

    /// Remove any sandbox/container/image this runtime knows about that's
    /// no longer referenced — orphaned pods (deleted from the apiserver
    /// while nodelet wasn't watching) and unused images. `live_pod_keys` is
    /// every `namespace/name` currently bound to this node per the
    /// apiserver. Default: no-op — nothing real to collect (mock).
    async fn gc(&self, live_pod_keys: &std::collections::HashSet<String>) -> anyhow::Result<()> {
        let _ = live_pod_keys;
        Ok(())
    }
}
