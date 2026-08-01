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

#[derive(Clone, Debug, Default)]
pub struct ContainerRuntimeStatus {
    pub name: String,
    pub image: String,
    /// CRI's own `Container.image_ref` (round 52; found in round 50's
    /// re-audit) — a digested reference to the image actually in use
    /// (e.g. `docker.io/library/nginx@sha256:...`), mapped to
    /// `containerStatuses[].imageID`. Empty when the runtime hasn't
    /// reported one (matches upstream's own "no image ID known yet"
    /// state — never fabricated).
    pub image_id: String,
    pub ready: bool,
    pub running: bool,
    pub container_id: Option<String>,
    /// Cumulative restart count, matching `PodStatus.containerStatuses[].restartCount`.
    pub restart_count: u32,
    /// Exit code from the container's last run, if it has ever exited —
    /// `None` means it hasn't (never started, or currently running).
    /// `build_pod_status()` uses `Some`/`None` here to decide whether a
    /// non-running container gets `state.terminated` (has run before) or
    /// `state.waiting` (never has) — see `pods.rs`.
    pub exit_code: Option<i32>,
    /// CRI's own `ContainerStatus.reason` (e.g. `"OOMKilled"`), falling
    /// back to `"Completed"`/`"Error"` (matching real kubelet) when the
    /// runtime didn't report one. Empty/meaningless while `exit_code` is
    /// `None`.
    pub reason: String,
    pub finished_at: Option<Timestamp>,
    /// Content of the container's `terminationMessagePath` file (round
    /// 24), read from the host-mounted copy `runtime/cri.rs`'s
    /// `create_and_start_container()` sets up — empty if the container
    /// never wrote one, is still running, or never ran at all. Only the
    /// `File` policy is implemented; `FallbackToLogsOnError` (reading the
    /// container's own log tail when the file is empty and the exit code
    /// is nonzero) is a documented, deliberate simplification not
    /// implemented this round — see `docs/GAP_CLOSURE.md`'s round 24
    /// notes.
    pub termination_message: String,
    /// Only meaningful for entries in `RuntimeStatus::init_containers` —
    /// whether this is a native sidecar (`initContainers[].restartPolicy:
    /// "Always"`, round 36). Unlike a regular init container (whose
    /// readiness is just "is it running," and which never affects the
    /// pod's overall `Ready`), a sidecar gets real probe-based readiness
    /// folded into `Ready`/`ContainersReady`, same as an app container —
    /// see `pods.rs::build_pod_status()`.
    pub is_restartable_sidecar: bool,
    /// In-place pod vertical scaling status reporting (round 43; the
    /// deferred half of round 42's resize arc) — `resources` is the
    /// container's *actual* currently-applied requests/limits (`None` for
    /// init/ephemeral containers this round; app containers only), mapped
    /// to `containerStatuses[].resources`. `allocated_resources` is what
    /// the current pod spec is asking for right now — nodelet has no
    /// admission/deferral layer, so this always mirrors the live spec
    /// rather than some separately-gated "accepted" value — mapped to
    /// `containerStatuses[].allocatedResources`.
    pub resources: Option<k8s_openapi::api::core::v1::ResourceRequirements>,
    pub allocated_resources: Option<std::collections::BTreeMap<String, k8s_openapi::apimachinery::pkg::api::resource::Quantity>>,
    /// CRI's own `ContainerStatus.stop_signal` (round 66; GA 1.33's
    /// `lifecycle.stopSignal`/`ContainerStatus.stopSignal`), mapped to
    /// `containerStatuses[].stopSignal` — the *effective* signal the
    /// runtime will use to stop this container. Only populated for
    /// containers whose full `ContainerStatus` was already being fetched
    /// for other reasons (currently: exited ones, for termination
    /// details) — a healthy running container doesn't pay an extra RPC
    /// just for this field, matching this codebase's low-idle-cost design
    /// throughout. `None` for a still-running container is a documented
    /// scope limitation, not "unset/RuntimeDefault."
    pub stop_signal: Option<String>,
    /// Round 75; found in round 73's crash-loop backoff work. `Some` only
    /// while a container that keeps exiting is currently in its backoff
    /// window (`waiting_reason_override` below always `Some` alongside
    /// this) or once at least one restart has actually happened — the
    /// previous instance's terminated details, mapped to
    /// `containerStatuses[].lastState`. `None` means either this
    /// container has never been replaced, or (mock runtime) this concept
    /// isn't tracked at all.
    pub last_terminated: Option<TerminatedInfo>,
    /// Round 75: overrides the caller's own default waiting reason
    /// (`"ContainerCreating"`/`"PodInitializing"`) — currently only ever
    /// `Some("CrashLoopBackOff")`, set while `exit_code` is deliberately
    /// left `None` here (suppressing the usual `Terminated` state) so a
    /// backing-off container reports `Waiting` instead, with its real
    /// exit details moved into `last_terminated` above rather than shown
    /// as the *current* state — matching real kubelet's own display.
    pub waiting_reason_override: Option<String>,
}

/// One container instance's terminated details, captured at the moment
/// it's about to be replaced by a fresh instance (or, while a
/// crash-looping container is mid-backoff, read live off the still-present
/// exited instance) — feeds `containerStatuses[].lastState` (round 75).
#[derive(Clone, Debug, Default)]
pub struct TerminatedInfo {
    pub container_id: Option<String>,
    pub exit_code: i32,
    pub reason: String,
    pub finished_at: Option<Timestamp>,
    pub message: String,
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
    /// the old one gone. `grace_period_seconds` (round 44; found in round
    /// 35's re-audit) is the probe's own `terminationGracePeriodSeconds`
    /// override if set, else the pod's own — computed by the caller
    /// (`probes.rs`), which has the `Probe`/`Pod` objects this trait-level
    /// method doesn't. Default: no-op — nothing real to restart (mock).
    async fn restart_container(&self, namespace: &str, name: &str, container: &str, grace_period_seconds: i64) -> anyhow::Result<()> {
        let _ = (namespace, name, container, grace_period_seconds);
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

    /// Extended-resource name (e.g. `nvidia.com/gpu`) -> healthy device
    /// count, from any registered device plugins (`cri` feature only —
    /// see `device_plugins.rs`). Feeds `Node.status.capacity`/`.allocatable`
    /// (`node.rs::build_status`). Default: empty (mock has no device
    /// plugins to query).
    fn device_plugin_capacity(&self) -> std::collections::BTreeMap<String, u64> {
        std::collections::BTreeMap::new()
    }

    /// Cached images this runtime currently has on disk — feeds
    /// `Node.status.images` (`node.rs::build_status`, round 33). Real
    /// kubelet caps this at 50 (`--node-status-max-images`) and sorts
    /// largest-first; both are `node.rs`'s job, not the runtime's, so
    /// this just reports everything it has. Default: empty (mock has no
    /// real image cache to report).
    async fn node_images(&self) -> anyhow::Result<Vec<NodeImage>> {
        Ok(Vec::new())
    }

    /// Every `(driver, volume_handle)` pair a CSI volume currently
    /// mounted by a pod on this node is backed by (round 34) — feeds
    /// `Node.status.volumesInUse`/`.volumesAttached`
    /// (`node.rs::build_status`). **Scoped to CSI volumes only** — this
    /// project has never supported the legacy in-tree volume plugins
    /// these fields originally existed for (`hostPath` is explicitly
    /// unsupported elsewhere), and CSI's own attach coordination (round
    /// 19) already uses `VolumeAttachment` objects directly rather than
    /// reading these fields back — so this is purely for *other*
    /// components (an older-style attach/detach controller path) that
    /// might read them, not something nodelet's own CSI logic depends
    /// on. Default: empty (mock has no CSI volumes to report).
    fn mounted_csi_volumes(&self) -> Vec<(String, String)> {
        Vec::new()
    }

    /// RuntimeClass handlers this runtime has discovered — feeds
    /// `Node.status.runtimeHandlers` (round 53; found in round 50's
    /// re-audit), via CRI's own runtime-level `Status` RPC. Used by
    /// RuntimeClass-aware tooling to confirm a handler actually exists on
    /// a node before scheduling to it. Default: empty (mock has no real
    /// runtime handlers to discover).
    async fn runtime_handlers(&self) -> anyhow::Result<Vec<RuntimeHandlerInfo>> {
        Ok(Vec::new())
    }

    /// Current CPU/memory/device allocation for every pod this runtime is
    /// actively managing — feeds the PodResources API's `List`/`Get` RPCs
    /// (round 74; found in round 72's re-audit). Default: empty (mock has
    /// no CPU/Memory/Device Manager state to report — those are `cri`-only
    /// concepts in this codebase).
    async fn pod_resources_snapshot(&self) -> Vec<PodResourcesEntry> {
        Vec::new()
    }

    /// Node-wide allocatable resources in the same shape `pod_resources_snapshot()`
    /// reports per-container — feeds the PodResources API's
    /// `GetAllocatableResources` RPC. Default: empty.
    fn allocatable_resources(&self) -> AllocatableResourcesSnapshot {
        AllocatableResourcesSnapshot::default()
    }
}

/// One CRI runtime handler, in the shape `Node.status.runtimeHandlers`
/// needs (round 53) — `name` empty denotes the default handler, matching
/// both CRI's and the Kubernetes API's own convention for this field.
#[derive(Clone, Debug, Default)]
pub struct RuntimeHandlerInfo {
    pub name: String,
    pub recursive_read_only_mounts: bool,
    pub user_namespaces: bool,
}

/// One pod's worth of `pod_resources_snapshot()` output (round 74) — kept
/// as nodelet's own type, not the generated PodResources proto type, for
/// the same reason `RuntimeHandlerInfo`/`UsageStats` are: usable from the
/// trait without a `cri`-feature bound on the trait itself.
#[derive(Clone, Debug, Default)]
pub struct PodResourcesEntry {
    pub namespace: String,
    pub name: String,
    pub containers: Vec<ContainerResourcesEntry>,
}

/// One container's currently-assigned CPU/device/pinned-memory resources.
/// `cpu_ids` is empty unless CPU Manager gave this container exclusive
/// cores; `memory` is empty unless Memory Manager pinned it to a NUMA
/// node; `devices` is empty unless it holds a device-plugin allocation.
/// **Dynamic Resource Allocation claims are deliberately not represented
/// here** — `PreparedPodClaim`s are recomputed fresh on every reconcile
/// (see `runtime/cri/claims.rs`) rather than kept in a queryable side
/// table the way CPU/Memory/device-plugin state is, so surfacing them
/// here would need a new persisted table of its own; a documented scope
/// limitation, not silently dropped.
#[derive(Clone, Debug, Default)]
pub struct ContainerResourcesEntry {
    pub name: String,
    pub cpu_ids: Vec<i64>,
    /// `(resource_name, device_ids)`, e.g. `("nvidia.com/gpu", ["gpu0"])`.
    pub devices: Vec<(String, Vec<String>)>,
    /// `(numa_node, bytes)` — at most one entry in this codebase's own
    /// Memory Manager (never spans multiple nodes, see `memory_manager.rs`).
    pub memory: Vec<(u32, u64)>,
}

/// Node-wide allocatable resources (round 74) — same shape as
/// `ContainerResourcesEntry` minus the container name, for
/// `GetAllocatableResources`.
#[derive(Clone, Debug, Default)]
pub struct AllocatableResourcesSnapshot {
    pub cpu_ids: Vec<i64>,
    pub devices: Vec<(String, Vec<String>)>,
    pub memory: Vec<(u32, u64)>,
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

#[derive(Clone, Debug, Default)]
pub struct PodUsage {
    pub namespace: String,
    pub name: String,
    pub uid: String,
    pub pod: UsageStats,
    pub containers: Vec<ContainerUsage>,
    /// Local ephemeral storage usage (round 49; the deferred half of
    /// round 48's arc) — every container's CRI-reported writable-layer
    /// usage, plus nodelet's own materialized volume directory (emptyDir/
    /// ConfigMap/Secret/downwardAPI/projected — everything under
    /// `VOLUME_ROOT/<uid>`). `None` when nothing could be measured at all
    /// (mock runtime; a `cri` pod CRI hasn't measured yet). **Known
    /// scope limitation**: does not include container log file size
    /// (`/var/log/pods/...`) — real kubelet's own measurement does;
    /// nodelet's doesn't walk that directory yet.
    pub ephemeral_storage_usage_bytes: Option<u64>,
    /// Per-volume usage in bytes, keyed by `spec.volumes[].name` (round
    /// 67) — every subdirectory nodelet materializes under
    /// `VOLUME_ROOT/<uid>/volumes/`, regardless of volume kind. Only
    /// `emptyDir` volumes with `sizeLimit` set are ever checked against
    /// this (`eviction::first_empty_dir_over_limit()`); entries for other
    /// volume kinds are harmlessly present but never matched against
    /// anything. Empty (not `None`) when nothing could be measured —
    /// same "no volumes, nothing to report" shape as an empty `containers`
    /// list, not a distinct "measurement failed" state.
    pub empty_dir_usage_bytes: std::collections::HashMap<String, u64>,
}

/// One cached image, in the shape `Node.status.images` needs (round 33)
/// — `names` combines CRI's own `repo_tags`/`repo_digests` (every name
/// this image is known by), matching upstream's own `ContainerImage.names`
/// field.
#[derive(Clone, Debug)]
pub struct NodeImage {
    pub names: Vec<String>,
    pub size_bytes: u64,
}
