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
#[cfg(feature = "cri")]
pub mod csi;

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
    /// Status of `spec.initContainers`, in manifest order — reported
    /// separately so `PodStatus.initContainerStatuses` can be populated
    /// (kubectl's `Init:N/M` display and `kubectl describe` both read this,
    /// not `containerStatuses`). Empty for runtimes/pods with none.
    pub init_containers: Vec<ContainerRuntimeStatus>,
    /// Whether every init container has completed (or there are none) —
    /// drives `PodStatus`'s `Initialized` condition. `true` for runtimes/
    /// pods with no init-container concept (mock; any pod mid-app-startup
    /// with no init containers declared).
    pub initialized: bool,
    /// Status of `spec.ephemeralContainers` (added post-hoc via the
    /// `ephemeralcontainers` subresource, e.g. by `kubectl debug`) — reported
    /// separately so `PodStatus.ephemeralContainerStatuses` can be
    /// populated. Unlike init/app containers, these never affect the pod's
    /// phase. Empty for runtimes/pods with none.
    pub ephemeral_containers: Vec<ContainerRuntimeStatus>,
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

    /// Rotate any running container's log file once it exceeds
    /// `max_size_bytes`, keeping at most `max_files` (real kubelet's
    /// `--container-log-max-size`/`--container-log-max-files`). Default:
    /// no-op — nothing real to rotate (mock).
    async fn rotate_logs(&self, max_size_bytes: u64, max_files: u32) -> anyhow::Result<()> {
        let _ = (max_size_bytes, max_files);
        Ok(())
    }

    /// Absolute path to a container's log file on this node, or `None` if
    /// the container doesn't exist. Backs the kubelet-style HTTP(S)
    /// server's `/containerLogs` endpoint (`server::logs`, `cri` feature
    /// only). Default: unsupported (mock writes no real log file).
    async fn container_log_path(&self, namespace: &str, name: &str, container: &str) -> anyhow::Result<Option<String>> {
        let _ = (namespace, name, container);
        Ok(None)
    }

    /// Ask the runtime for a one-shot streaming URL to exec a command in a
    /// container — CRI's model: the runtime (containerd) runs its own tiny
    /// streaming server and hands back a URL for *this* exec session;
    /// nodelet's HTTP(S) server proxies the client's connection to it
    /// rather than implementing the SPDY/WebSocket exec protocol itself
    /// (see `server::exec`). Default: unsupported (mock has no real
    /// container to exec into).
    async fn exec_url(
        &self,
        namespace: &str,
        name: &str,
        container: &str,
        cmd: &[String],
        stdin: bool,
        stdout: bool,
        stderr: bool,
        tty: bool,
    ) -> anyhow::Result<String> {
        let _ = (namespace, name, container, cmd, stdin, stdout, stderr, tty);
        anyhow::bail!("exec is not supported by this runtime")
    }

    /// Same idea as `exec_url`, for attaching to a container's already-running
    /// process 1 instead of starting a new command.
    async fn attach_url(
        &self,
        namespace: &str,
        name: &str,
        container: &str,
        stdin: bool,
        stdout: bool,
        stderr: bool,
        tty: bool,
    ) -> anyhow::Result<String> {
        let _ = (namespace, name, container, stdin, stdout, stderr, tty);
        anyhow::bail!("attach is not supported by this runtime")
    }

    /// Same idea again, for forwarding TCP ports into a pod's network namespace.
    async fn port_forward_url(&self, namespace: &str, name: &str) -> anyhow::Result<String> {
        let _ = (namespace, name);
        anyhow::bail!("port-forward is not supported by this runtime")
    }

    /// Live CPU/memory usage for every pod this runtime knows about — backs
    /// the `/stats/summary` endpoint (`server::stats`, `cri` feature only).
    /// Default: empty (mock has no real containers to measure).
    async fn pod_usage_stats(&self) -> anyhow::Result<Vec<PodUsage>> {
        Ok(Vec::new())
    }
}

/// CPU/memory usage numbers in the same shape CRI's `CpuUsage`/`MemoryUsage`
/// report them — kept as nodelet's own type (not the generated CRI proto
/// type) so this is usable from the trait without a `cri`-feature bound on
/// the trait itself.
#[derive(Clone, Copy, Debug, Default)]
pub struct UsageStats {
    pub cpu_usage_nano_cores: Option<u64>,
    pub cpu_usage_core_nano_seconds: Option<u64>,
    pub memory_working_set_bytes: Option<u64>,
    pub memory_usage_bytes: Option<u64>,
    pub memory_rss_bytes: Option<u64>,
    pub memory_available_bytes: Option<u64>,
}

#[derive(Clone, Debug)]
pub struct ContainerUsage {
    pub name: String,
    pub stats: UsageStats,
}

#[derive(Clone, Debug)]
pub struct PodUsage {
    pub namespace: String,
    pub name: String,
    pub uid: String,
    pub pod: UsageStats,
    pub containers: Vec<ContainerUsage>,
}
