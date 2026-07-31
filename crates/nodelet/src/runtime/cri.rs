//! Real container runtime via the containerd/CRI gRPC API (`runtime.v1`).
//!
//! Implements the pod lifecycle against any CRI runtime (containerd, CRI-O):
//! RunPodSandbox → PullImage → CreateContainer → StartContainer, and teardown
//! via StopPodSandbox/RemovePodSandbox. Pods/containers are tagged with
//! `nodelet.dev/*` labels so every operation is idempotent (we look up existing
//! sandboxes/containers by label instead of tracking state ourselves).
//!
//! Status is **event-driven**: a background task subscribes to the CRI
//! `GetContainerEvents` stream and pushes changed pod keys onto a channel — no
//! PLEG-style per-second relisting.

#![allow(clippy::needless_question_mark)]

use super::{ContainerRuntimeStatus, Phase, PodRuntime, RuntimeStatus};
use anyhow::{bail, Context, Result};
use crate::eviction::QosClass;
use async_trait::async_trait;
use k8s_openapi::api::core::v1::{
    ConfigMap, Container, ContainerResizePolicy, EnvFromSource, EnvVarSource, EphemeralContainer, LifecycleHandler,
    PersistentVolume, PersistentVolumeClaim, Pod, PodSecurityContext, ResourceRequirements, Secret,
    SecretReference, SecurityContext, Service, Volume,
};
use k8s_openapi::api::node::v1::RuntimeClass;
use k8s_openapi::api::storage::v1::{CSIDriver, VolumeAttachment};
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use k8s_openapi::apimachinery::pkg::util::intstr::IntOrString;
use k8s_openapi::jiff::Timestamp;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use kube::api::{Api, ListParams};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use tonic::transport::{Channel, Endpoint, Uri};
use tracing::{debug, info, warn};

/// Generated CRI v1 types and gRPC clients (from proto/cri.proto).
pub mod v1 {
    tonic::include_proto!("runtime.v1");
}

/// Generated containerd top-level events API (from proto/containerd_events.proto).
pub mod events {
    tonic::include_proto!("containerd.services.events.v1");
}

use events::events_client::EventsClient;
use events::{SubscribeRequest, TaskEventContainerId};
use prost::Message as _;

use v1::image_service_client::ImageServiceClient;
use v1::runtime_service_client::RuntimeServiceClient;
use v1::{
    AuthConfig, Capability, ContainerConfig, ContainerFilter, ContainerMetadata, ContainerState,
    ContainerStatusRequest, CreateContainerRequest, DnsConfig, GetEventsRequest, ImageSpec, ImageStatusRequest, Int64Value,
    KeyValue, LinuxContainerConfig, LinuxContainerResources, LinuxContainerSecurityContext,
    LinuxPodSandboxConfig, LinuxSandboxSecurityContext, ListContainersRequest, ListImagesRequest,
    ListPodSandboxRequest, NamespaceMode, NamespaceOption, PodSandboxConfig, PodSandboxFilter,
    PodSandboxMetadata, PodSandboxStatusRequest, PullImageRequest, Mount, RemoveContainerRequest, StatusRequest, VersionRequest,
    RemoveImageRequest, RemovePodSandboxRequest, RunPodSandboxRequest, StartContainerRequest,
    StopContainerRequest, StopPodSandboxRequest, ExecSyncRequest, ReopenContainerLogRequest,
    ExecRequest, AttachRequest, PortForwardRequest, ListPodSandboxStatsRequest,
    UpdateContainerResourcesRequest, security_profile::ProfileType, SecurityProfile,
    IdMapping, UserNamespace,
};

/// Where ConfigMap/Secret volume contents get materialized on the host, one
/// subdirectory per pod UID — mirrors a real kubelet's
/// /var/lib/kubelet/pods/<uid>/volumes/ layout closely enough that this is
/// recognizable, without trying to be a drop-in match.
const VOLUME_ROOT: &str = "/var/lib/nodelet/pods";

/// `terminationMessagePath`'s effective cap (round 24) — real kubelet's own
/// `kubecontainer.MaxContainerTerminationMessageLength`. A container that
/// writes more than this to its termination-log file only has the last
/// this-many bytes read back (matching upstream: the most recent content
/// is what usually matters for a short human-readable failure reason, not
/// whatever was written first).
const MAX_TERMINATION_MESSAGE_BYTES: usize = 4096;

/// Where a container's `terminationMessagePath` file lives on the host —
/// bind-mounted into the container at container-creation time (see
/// `create_and_start_container()`) so nodelet can read it back after the
/// container exits without needing any runtime cooperation, the same
/// approach real kubelet itself uses (not a CRI-level concept at all).
fn termination_message_host_path(pod_uid: &str, container_name: &str) -> PathBuf {
    PathBuf::from(VOLUME_ROOT).join(pod_uid).join("termination-log").join(container_name)
}

/// Read a termination-message file's content, capped at
/// `MAX_TERMINATION_MESSAGE_BYTES` — keeping the *last* bytes if the file
/// is larger (a container's most recent write), not the first. Empty (not
/// an error) for a missing/unreadable file — the common case: most
/// containers never write one at all.
fn read_termination_message(path: &std::path::Path) -> String {
    match std::fs::read(path) {
        Ok(bytes) => {
            let start = bytes.len().saturating_sub(MAX_TERMINATION_MESSAGE_BYTES);
            String::from_utf8_lossy(&bytes[start..]).into_owned()
        }
        Err(_) => String::new(),
    }
}

const POD_UID_LABEL: &str = "nodelet.dev/pod-uid";
const POD_NAME_LABEL: &str = "nodelet.dev/pod-name";
const POD_NS_LABEL: &str = "nodelet.dev/pod-namespace";
const CTR_NAME_LABEL: &str = "nodelet.dev/container-name";
/// Present (value `"true"`) only on containers created from `spec.initContainers`
/// — lets status-building and GC tell them apart from app containers without
/// a second side table.
const CTR_INIT_LABEL: &str = "nodelet.dev/init";
/// Present (value `"true"`) only on containers created from
/// `spec.ephemeralContainers` (e.g. `kubectl debug`) — like `CTR_INIT_LABEL`,
/// lets status-building exclude them from the app-container phase logic:
/// unlike app/init containers, an ephemeral container exiting must never
/// affect the pod's phase.
const CTR_EPHEMERAL_LABEL: &str = "nodelet.dev/ephemeral";
/// Synthetic key in the volumes map for a pod's generated `/etc/hosts`
/// (`hostAliases`) — not a real Kubernetes volume, so it needs a name no
/// actual `spec.volumes[].name` could collide with.
const ETC_HOSTS_VOLUME_KEY: &str = "nodelet.dev/etc-hosts";

pub struct CriRuntime {
    rt: RuntimeServiceClient<Channel>,
    img: ImageServiceClient<Channel>,
    // Needed to resolve ConfigMap/Secret volumes (see resolve_volumes()) —
    // the CRI API has no concept of these, only host-path bind mounts, so
    // their contents have to be fetched from the apiserver and written to
    // disk ourselves before a container that mounts them can start.
    client: kube::Client,
    /// This node's name — needed to match this node's `VolumeAttachment`
    /// objects (`spec.nodeName`) when waiting on a CSI attach (see
    /// `resolve_csi_source()`).
    node_name: String,
    /// `--cluster-dns`/`--cluster-domain` equivalents (see `dns_config_for()`).
    cluster_dns: Vec<String>,
    cluster_domain: String,
    rx: Mutex<Option<UnboundedReceiver<String>>>,
    // sandbox_id -> the owning Pod's restartPolicy ("Always"/"OnFailure"/"Never"),
    // recorded whenever ensure_pod() runs. build_status() needs this to decide
    // whether an all-exited container set means the pod is genuinely done
    // (Never/OnFailure-with-zero-exit) or just mid-restart (Always — see the
    // module-level restart-on-exit comment on ensure_container). The
    // event-driven status() path has no Pod object to read it from directly
    // (only namespace+name), hence the side table instead of a parameter.
    restart_policies: Mutex<HashMap<String, String>>,
    /// `sandbox_id -> pod uid`, same reason/lifecycle as `restart_policies`
    /// (event-driven `status()` only gets namespace+name, no `Pod` object)
    /// — needed to find a container's `terminationMessagePath` host file
    /// (`termination_message_host_path()`), which is keyed by pod uid, not
    /// sandbox id.
    pod_uids: Mutex<HashMap<String, String>>,
    /// `sandbox_id -> names of init containers that are native sidecars`
    /// (round 36; `initContainers[].restartPolicy: "Always"`), same
    /// lifecycle/reason as `pod_uids` — `build_labeled_container_statuses()`'s
    /// event-driven callers only get namespace+name, no `Pod` object, but
    /// need to know which init containers to report probe-based readiness
    /// for and fold into the pod's overall `Ready`/`ContainersReady`
    /// (`pods.rs::build_pod_status()`), same as app containers already get.
    sidecar_names: Mutex<HashMap<String, HashSet<String>>>,
    /// Exclusive per-pod UID/GID range allocator for `spec.hostUsers: false`
    /// (round 25; see `userns.rs`) — keyed by pod uid, released alongside
    /// `pod_uids`/`restart_policies` on pod removal/orphan GC/stale-sandbox
    /// recreation.
    userns: crate::userns::UsernsAllocator,
    /// Node memory capacity in bytes (round 28) — the "machine memory
    /// capacity" input to `eviction::oom_score_adj()`'s Burstable-QoS
    /// scaling formula. Same source as `Node.status.capacity.memory`
    /// (`config.rs::detect_memory_bytes()` / `NODELET_MEMORY_BYTES`), not
    /// re-read from `/proc/meminfo` independently.
    node_memory_bytes: i64,
    /// Node CPU capacity in millicores (round 44; found in round 35's
    /// re-audit) — the fallback value `resolve_env_var_source()`'s
    /// `resourceFieldRef` handling uses for `limits.cpu` when the
    /// container itself has no CPU limit set, matching real kubelet's own
    /// documented behavior (an unset limit resolves to the *node's*
    /// capacity, not zero/unlimited). Same source as
    /// `Node.status.capacity.cpu` (`config.rs`'s `cpu_cores` /
    /// `NODELET_CPU`).
    node_cpu_millicores: i64,
    /// This CRI runtime's own name (e.g. `"containerd"`), from a one-time
    /// `Version` RPC call made in `connect()` (round 57; found in round
    /// 54's re-audit) — real kubelet always formats `ContainerStatus.containerID`/
    /// `state.terminated.containerID` as `<runtimeName>://<id>`, built
    /// from this exact same call. `"unknown"` if the call ever fails
    /// (best-effort — a cosmetic prefix isn't worth failing `connect()`
    /// over).
    runtime_name: String,
    /// `"sandbox_id/container_name" -> cumulative restart count`, mirroring
    /// `PodStatus.containerStatuses[].restartCount`. Side table for the same
    /// reason `restart_policies` is one: CRI's `ListContainers` has no
    /// concept of "how many times has this logical container restarted" —
    /// each restart is a brand-new container id, so nodelet has to count.
    restart_counts: Mutex<HashMap<String, u32>>,
    /// CSI Node-service clients for `PersistentVolumeClaim` volumes (see
    /// `runtime/csi.rs`) — empty at startup (unless seeded via
    /// `NODELET_CSI_DRIVERS`) means every PVC volume is skipped with a
    /// warning until a driver registers (see `plugin_registry.rs`) or is
    /// statically configured. `Arc` so `CriRuntime::connect()` can hand the
    /// same instance to the plugin-registry watcher task it spawns.
    csi: Arc<crate::runtime::csi::CsiDrivers>,
    /// Device plugin inventory/allocation state (see `device_plugins.rs`)
    /// — populated entirely via dynamic registration (no static-config
    /// equivalent to `NODELET_CSI_DRIVERS`; a device plugin always needs
    /// its own `ListAndWatch` stream to report device health, which a
    /// static socket path alone can't provide).
    device_plugins: Arc<crate::device_plugins::DevicePlugins>,
    /// `"sandbox_id/container_name" -> [(resource_name, device_ids)]` for
    /// every device-plugin allocation currently backing a container — the
    /// counterpart to `restart_counts`'s side table, needed so a
    /// container's devices can be released back to the pool when it's
    /// removed/restarted, without re-deriving which devices it held from
    /// anywhere else (CRI itself has no concept of device-plugin resources
    /// to ask back).
    device_allocations: Mutex<HashMap<String, Vec<(String, Vec<String>)>>>,
    /// Exclusive CPU pinning for Guaranteed-QoS containers (see
    /// `cpu_manager.rs`) — `None` when `NODELET_CPU_MANAGER_POLICY` is
    /// unset/`none` (the default), meaning every container's `cpuset_cpus`
    /// is left unset ("unconstrained"), exactly the pre-round-15 behavior.
    cpu_manager: Option<crate::cpu_manager::CpuManager>,
    /// NUMA memory pinning for Guaranteed-QoS containers (see
    /// `memory_manager.rs`) — `None` when `NODELET_MEMORY_MANAGER_POLICY`
    /// is unset/`none` (the default), meaning every container's
    /// `cpuset_mems` is left unset ("unconstrained").
    memory_manager: Option<crate::memory_manager::MemoryManager>,
    /// `"sandbox_id/container_name" -> (container_id, last-applied
    /// LinuxContainerResources)` for every currently-running container,
    /// kept only so CPU Manager's retroactive shared-pool refresh
    /// (`refresh_shared_pool_cpusets()`) can call `UpdateContainerResources`
    /// with everything unchanged except `cpuset_cpus` — CRI's `ListContainers`
    /// doesn't expose a container's currently-applied resources in any
    /// structured, cross-runtime way, so this is nodelet's own record of
    /// "what did I last tell the runtime this container's resources were."
    container_resources: Mutex<HashMap<String, (String, LinuxContainerResources)>>,
    /// Resize status reporting (round 43; the deferred half of round 42's
    /// arc). Same `"sandbox_id/container_name"` key as `container_resources`
    /// (and the same reasoning: `build_status()`'s event-driven path has no
    /// `Pod` object to read this from directly).
    /// `applied_resources`: the container's ORIGINAL k8s `ResourceRequirements`
    /// last successfully applied (create, or a successful in-place resize) —
    /// reported as `containerStatuses[].resources` (the *actual* running
    /// value). Tracking the k8s-native struct directly (rather than
    /// reverse-converting `container_resources`' CRI-form
    /// `LinuxContainerResources` back into `Quantity` strings) avoids a
    /// lossy round trip.
    applied_resources: Mutex<HashMap<String, ResourceRequirements>>,
    /// `spec_resources`: the current pod spec's own requests for this
    /// container, refreshed on every `ensure_container()` call regardless
    /// of whether a resize succeeded, failed, or wasn't needed — reported
    /// as `containerStatuses[].allocatedResources` (what nodelet is
    /// currently trying to admit). Nodelet has no admission/deferral layer
    /// at all, so this always just mirrors the live pod spec rather than
    /// some separately-gated "accepted" value.
    spec_resources: Mutex<HashMap<String, BTreeMap<String, Quantity>>>,
    /// Real kubelet's `--topology-manager-policy` (see `topology.rs`) —
    /// `None` (upstream's own default) means the alignment computation in
    /// `create_and_start_container` is skipped entirely.
    topology_policy: crate::topology::TopologyManagerPolicy,
    /// NUMA node -> the CPU IDs it owns, read once at startup (NUMA
    /// topology doesn't change at runtime on any real hardware nodelet
    /// targets). Empty on a host with no NUMA info at all (most
    /// single-socket edge devices) — Topology Manager then never finds an
    /// alignment, which `BestEffort` tolerates and `Restricted`/
    /// `SingleNumaNode` would reject on, so those policies only make
    /// sense on hardware that actually reports NUMA topology.
    numa_topology: BTreeMap<u32, BTreeSet<u32>>,
}

/// Identity extracted from a Pod object.
struct PodId {
    namespace: String,
    name: String,
    uid: String,
    host_network: bool,
    /// `spec.hostUsers` — `true` (the default, matching upstream) means no
    /// user namespace at all; only an explicit `false` triggers an
    /// exclusive UID/GID range allocation (see `userns.rs`, round 25).
    host_users: bool,
    /// `spec.hostPID`/`spec.hostIPC` (round 40) — share the host's PID/IPC
    /// namespace instead of an isolated one.
    host_pid: bool,
    host_ipc: bool,
    /// `spec.shareProcessNamespace` (round 40) — every container in the pod
    /// shares one PID namespace instead of each getting its own. Ignored
    /// (moot) when `host_pid` is also true.
    share_process_namespace: bool,
}

fn pod_id(pod: &Pod) -> PodId {
    let namespace = pod.metadata.namespace.clone().unwrap_or_else(|| "default".to_string());
    let name = pod.metadata.name.clone().unwrap_or_default();
    let uid = pod
        .metadata
        .uid
        .clone()
        .unwrap_or_else(|| format!("{namespace}_{name}"));
    let spec = pod.spec.as_ref();
    let host_network = spec.and_then(|s| s.host_network).unwrap_or(false);
    let host_users = spec.and_then(|s| s.host_users).unwrap_or(true);
    let host_pid = spec.and_then(|s| s.host_pid).unwrap_or(false);
    let host_ipc = spec.and_then(|s| s.host_ipc).unwrap_or(false);
    let share_process_namespace = spec.and_then(|s| s.share_process_namespace).unwrap_or(false);
    PodId { namespace, name, uid, host_network, host_users, host_pid, host_ipc, share_process_namespace }
}

/// Real kubelet's PID-namespace mode: `hostPID` wins outright (share the
/// host's), then `shareProcessNamespace` (every container in the pod shares
/// one), otherwise each container gets its own — CRI's proto default (unset
/// = `POD`) is the *opposite* of this and was round 40's actual correctness
/// finding, not just a missing feature (see `docs/GAP_CLOSURE.md` round 39).
fn pid_namespace_mode(host_pid: bool, share_process_namespace: bool) -> NamespaceMode {
    if host_pid {
        NamespaceMode::Node
    } else if share_process_namespace {
        NamespaceMode::Pod
    } else {
        NamespaceMode::Container
    }
}

/// What ensure_container() should do about an already-existing container
/// with the target name, given its CRI state and the pod's restartPolicy.
/// Pulled out as a pure decision (see restart_decision()) specifically so
/// the restart-on-exit fix (crates the whole coredns pile-up traced back
/// to) has a unit-testable matrix instead of only being verifiable by
/// hand against a real cluster.
#[derive(Debug, PartialEq, Eq)]
enum RestartDecision {
    /// Already running — leave it alone.
    AlreadyRunning,
    /// Not running, but restartPolicy: Never means it's done for good —
    /// leave it alone (Job-style one-shot semantics).
    LeaveTerminated,
    /// Not running and this pod is allowed to restart — remove the stale
    /// container and create a fresh one.
    NeedsRestart,
}

/// What ensure_pod() should do about a sandbox lookup result, given its CRI
/// state. Pulled out as a pure decision for the same reason as
/// restart_decision() above: this exact bug (reusing a dead sandbox forever
/// after a reboot) was only found by hand, against a real device, and
/// deserves a matrix that doesn't require one to catch again.
#[derive(Debug, PartialEq, Eq)]
enum SandboxDecision {
    /// A ready sandbox exists — use it as-is.
    Reuse,
    /// A sandbox exists but isn't ready (its task/pause process is gone,
    /// e.g. after a reboot) — tear it down and create a fresh one.
    RecreateStale,
    /// No sandbox at all — create one.
    CreateFresh,
}

fn sandbox_reuse_decision(found: Option<i32>, ready_state: i32) -> SandboxDecision {
    match found {
        Some(s) if s == ready_state => SandboxDecision::Reuse,
        Some(_) => SandboxDecision::RecreateStale,
        None => SandboxDecision::CreateFresh,
    }
}

/// What `ensure_init_containers()` should do about one init container, given
/// its CRI state (if it exists at all) and — if exited — its exit code.
/// Pulled out as a pure decision for the same reason `restart_decision()`
/// and `compute_phase()` are: this is the exact logic that decides whether
/// init containers gate app containers correctly, and deserves a matrix
/// that doesn't require a live CRI socket to verify.
#[derive(Debug, PartialEq, Eq)]
enum InitContainerDecision {
    /// Doesn't exist yet — create and start it.
    Create,
    /// Running — wait for it.
    StillRunning,
    /// Exited zero — this one's done; check the next init container.
    Done,
    /// Exited nonzero and this pod is allowed to restart — remove it so a
    /// fresh one gets created.
    Retry,
    /// Exited nonzero under `restartPolicy: Never` — terminal.
    Failed,
    /// Neither running nor exited (e.g. still being created) — wait.
    Waiting,
}

fn init_container_decision(
    existing_state: Option<i32>,
    running_state: i32,
    exited_state: i32,
    exit_code: i32,
    restart_policy: &str,
) -> InitContainerDecision {
    match existing_state {
        None => InitContainerDecision::Create,
        Some(s) if s == running_state => InitContainerDecision::StillRunning,
        Some(s) if s == exited_state => {
            if exit_code == 0 {
                InitContainerDecision::Done
            } else if restart_policy == "Never" {
                InitContainerDecision::Failed
            } else {
                InitContainerDecision::Retry
            }
        }
        Some(_) => InitContainerDecision::Waiting,
    }
}

/// Where `ensure_init_containers()` left off.
#[derive(Debug, PartialEq, Eq)]
enum InitProgress {
    /// The next not-yet-done init container was just created, or is still
    /// running, or is done and something after it isn't — either way, the
    /// app containers must not start yet.
    Waiting,
    /// An init container exited nonzero under `restartPolicy: Never` —
    /// terminal, matches kubelet reporting the whole Pod `Failed`.
    Failed(String),
    /// Every init container has exited zero, in order — start the app containers.
    AllComplete,
}

/// What `ensure_init_containers()` should do about one native sidecar
/// container (`initContainers[].restartPolicy: "Always"`, round 36),
/// given its CRI state if it exists at all. Unlike a regular init
/// container (`InitContainerDecision`), a sidecar never blocks later
/// containers on its own *exit* — only on having been created at all —
/// and restarts on exit like a normal container, indefinitely, for the
/// pod's whole lifetime. Pulled out as a pure decision for the same
/// reason `init_container_decision()`/`restart_decision()` are.
#[derive(Debug, PartialEq, Eq)]
enum SidecarInitDecision {
    /// Doesn't exist yet — create it, and block later containers until
    /// it's at least been created (matching upstream's "wait for Started"
    /// gate, approximated here as "creation issued").
    Create,
    /// Exited — restart it, but don't block later containers on this.
    NeedsRestart,
    /// Running (or some other transient CRI state) — already started;
    /// don't block later containers.
    Started,
}

fn sidecar_init_decision(existing_state: Option<i32>, running_state: i32, exited_state: i32) -> SidecarInitDecision {
    match existing_state {
        None => SidecarInitDecision::Create,
        Some(s) if s == exited_state => SidecarInitDecision::NeedsRestart,
        Some(s) if s == running_state => SidecarInitDecision::Started,
        Some(_) => SidecarInitDecision::Started, // some other transient CRI state — don't block on it either
    }
}

fn restart_decision(existing_state: Option<i32>, running_state: i32, restart_policy: &str) -> RestartDecision {
    match existing_state {
        None => RestartDecision::NeedsRestart, // no existing container at all — same code path as a genuine restart
        Some(s) if s == running_state => RestartDecision::AlreadyRunning,
        Some(_) if restart_policy == "Never" => RestartDecision::LeaveTerminated,
        Some(_) => RestartDecision::NeedsRestart,
    }
}

/// Pod-level phase from container CRI states + restartPolicy. See the
/// long comment on build_status()'s call site for why restartPolicy has to
/// factor in here — reporting Succeeded for a restartPolicy: Always pod
/// whose container merely exited is the bug that drove unbounded coredns
/// pod creation (Kubernetes' ReplicaSet controller treats Succeeded/Failed
/// pods as permanently inactive and replaces them).
/// `any_failed` only matters (and is only computed by the caller) when
/// `restart_policy == "Never"` and every container has exited — otherwise a
/// nonzero exit just means "will be restarted", not "this pod failed".
/// Before this, `restartPolicy: Never` always reported `Succeeded` even when
/// a container exited nonzero — a Job-style pod that actually failed looked
/// like it succeeded.
fn compute_phase(any_running: bool, all_exited: bool, any_failed: bool, restart_policy: &str) -> Phase {
    if any_running {
        Phase::Running
    } else if all_exited && restart_policy == "Never" {
        if any_failed {
            Phase::Failed
        } else {
            Phase::Succeeded
        }
    } else {
        Phase::Pending
    }
}

/// Pure restart-count-table logic, pulled out of `CriRuntime`'s methods so
/// it's unit-testable without a real CRI socket/kube client (a `CriRuntime`
/// can't be constructed without both).
fn restart_count_key(sandbox_id: &str, container_name: &str) -> String {
    format!("{sandbox_id}/{container_name}")
}

fn restart_count_from(counts: &HashMap<String, u32>, sandbox_id: &str, container_name: &str) -> u32 {
    counts.get(&restart_count_key(sandbox_id, container_name)).copied().unwrap_or(0)
}

fn bump_restart_count_in(counts: &mut HashMap<String, u32>, sandbox_id: &str, container_name: &str) -> u32 {
    let entry = counts.entry(restart_count_key(sandbox_id, container_name)).or_insert(0);
    *entry += 1;
    *entry
}

fn clear_restart_counts_in(counts: &mut HashMap<String, u32>, sandbox_id: &str) {
    let prefix = format!("{sandbox_id}/");
    counts.retain(|k, _| !k.starts_with(&prefix));
}

/// Real kubelet default when `terminationGracePeriodSeconds` is unset (or
/// explicitly negative, which the API otherwise allows through): 30s.
fn termination_grace_seconds(pod: &Pod) -> i64 {
    match pod.spec.as_ref().and_then(|s| s.termination_grace_period_seconds) {
        Some(s) if s >= 0 => s,
        _ => 30,
    }
}

/// A volume `resolve_volumes()` has resolved to something mountable —
/// either a host path nodelet materialized itself (every volume kind
/// before round 32: ConfigMap/Secret/emptyDir/downwardAPI/projected/PVC/
/// ephemeral, all bind-mounted from a real host directory), or an image
/// reference for `volumeSource.image` (round 32) — CRI's `Mount.image`
/// field handles those directly, with no host path involved at all
/// (`host_path` must stay empty per the proto's own contract).
#[derive(Clone, Debug)]
enum ResolvedVolume {
    HostPath(PathBuf),
    Image { image_ref: String },
}

/// Build CRI `Mount` entries for a container's volumeMounts against the
/// pod's already-resolved volume name -> mount source map (see
/// resolve_volumes()). A mount naming a volume that isn't in the map
/// (unsupported volume type, or the ConfigMap/Secret fetch failed) is
/// silently dropped — pulled out as a pure function specifically to make
/// that behavior, and subPath/readOnly handling, unit-testable without a
/// real CRI socket.
fn build_mounts(
    volume_mounts: &[k8s_openapi::api::core::v1::VolumeMount],
    volumes: &HashMap<String, ResolvedVolume>,
) -> Vec<Mount> {
    volume_mounts
        .iter()
        .filter_map(|vm| match volumes.get(&vm.name)? {
            ResolvedVolume::HostPath(host_dir) => {
                let host_path = match &vm.sub_path {
                    Some(sub) => host_dir.join(sub),
                    None => host_dir.clone(),
                };
                Some(Mount {
                    container_path: vm.mount_path.clone(),
                    host_path: host_path.to_string_lossy().into_owned(),
                    readonly: vm.read_only.unwrap_or(false),
                    ..Default::default()
                })
            }
            ResolvedVolume::Image { image_ref } => Some(Mount {
                container_path: vm.mount_path.clone(),
                // Must stay empty — CRI's Mount.image and Mount.host_path
                // are mutually exclusive by the proto's own contract.
                host_path: String::new(),
                readonly: true, // image volumes are always read-only, matching the KEP
                image: Some(ImageSpec { image: image_ref.clone(), ..Default::default() }),
                // The container's own volumeMounts[].subPath, same field
                // regular volumes already use to select a subdirectory —
                // for an image-backed volume it selects a path *within*
                // the mounted image instead (CRI's `image_sub_path`).
                image_sub_path: vm.sub_path.clone().unwrap_or_default(),
                ..Default::default()
            }),
        })
        .collect()
}

/// kubelet's fixed CPU CFS period (`--cpu-cfs-quota-period`'s default, 100ms
/// in microseconds) — quota is computed against this, not configurable here.
const CPU_CFS_QUOTA_PERIOD_US: i64 = 100_000;

/// Parse a Kubernetes `Quantity` suffix (`Ki`/`Mi`/`Gi`/`Ti` binary, `k`/`M`/`G`/`T`
/// decimal, or bare). Uses f64 — imprecise at the very top of i64 range, which
/// doesn't matter for cpu/memory quantities on any real machine.
fn parse_quantity(s: &str) -> Option<f64> {
    const BINARY: [(&str, f64); 4] =
        [("Ki", 1024.0), ("Mi", 1024.0 * 1024.0), ("Gi", 1024.0 * 1024.0 * 1024.0), ("Ti", 1024.0 * 1024.0 * 1024.0 * 1024.0)];
    const DECIMAL: [(&str, f64); 4] = [("k", 1e3), ("M", 1e6), ("G", 1e9), ("T", 1e12)];
    let s = s.trim();
    for (suf, mult) in BINARY.into_iter().chain(DECIMAL) {
        if let Some(num) = s.strip_suffix(suf) {
            return num.parse::<f64>().ok().map(|n| n * mult);
        }
    }
    s.parse::<f64>().ok()
}

/// Whether a CSI driver requires an attach before Stage/Publish — pure
/// wrapper around `CSIDriver.spec.attachRequired` so the "assume yes
/// if the field/object is missing" default (matching upstream) is
/// unit-testable without a cluster. `None` means no `CSIDriver` object
/// exists for this driver name at all.
fn attach_required(driver: Option<&CSIDriver>) -> bool {
    driver.and_then(|d| d.spec.attach_required) != Some(false)
}

/// Find the `VolumeAttachment` (if any) describing `driver` attaching
/// `pv_name` to `node_name` — a pure search so the matching logic is
/// unit-testable without listing real cluster objects. `VolumeAttachment`
/// names are generated (hashed) by the attach/detach controller, not
/// derivable from `(driver, node, pv)`, so this has to scan rather than
/// `.get()` by name.
fn find_volume_attachment<'a>(
    attachments: &'a [VolumeAttachment],
    driver: &str,
    node_name: &str,
    pv_name: &str,
) -> Option<&'a VolumeAttachment> {
    attachments.iter().find(|a| {
        a.spec.attacher == driver
            && a.spec.node_name == node_name
            && a.spec.source.persistent_volume_name.as_deref() == Some(pv_name)
    })
}

/// Extract the `publish_context` for Stage/Publish from an attached
/// `VolumeAttachment` — `None` if it isn't attached yet
/// (`status.attached == false`, or no `status` at all: the
/// external-attacher hasn't finished `ControllerPublishVolume` yet).
fn attachment_publish_context(attachment: &VolumeAttachment) -> Option<HashMap<String, String>> {
    let status = attachment.status.as_ref()?;
    if !status.attached {
        return None;
    }
    Some(status.attachment_metadata.clone().unwrap_or_default().into_iter().collect())
}

/// Every non-cpu/memory resource in `limits`, as `(name, count)` — a pure
/// extraction so "does this container ask for an extended resource" is
/// unit-testable without a live `DevicePlugins` registry. Whether nodelet
/// actually has a driver for a given name (and so whether it's really a
/// device-plugin resource, as opposed to something with no kubelet-side
/// meaning at all) is decided by the caller via
/// `DevicePlugins::resource_configured()`.
fn extended_resource_requests(limits: Option<&BTreeMap<String, Quantity>>) -> Vec<(String, u64)> {
    let Some(limits) = limits else { return Vec::new() };
    limits
        .iter()
        .filter(|(name, _)| name.as_str() != "cpu" && name.as_str() != "memory")
        .filter_map(|(name, q)| parse_quantity(&q.0).map(|v| (name.clone(), v.round().max(0.0) as u64)))
        .collect()
}

/// A cpu Quantity as millicores: `"500m"` -> 500, `"2"` -> 2000, `"0.5"` -> 500.
fn parse_cpu_millicores(q: &Quantity) -> Option<i64> {
    let s = q.0.trim();
    if let Some(m) = s.strip_suffix('m') {
        return m.parse::<f64>().ok().map(|v| v.round() as i64);
    }
    parse_quantity(s).map(|cores| (cores * 1000.0).round() as i64)
}

/// A memory Quantity as bytes.
fn parse_memory_bytes(q: &Quantity) -> Option<i64> {
    parse_quantity(&q.0).map(|b| b.round() as i64)
}

/// The mutable tag on an image reference, if any — `None` for a bare
/// digest reference (`repo@sha256:...`) or a plain repo name with no tag
/// at all. Only looks at the segment after the last `/`, so a registry
/// host:port (e.g. `myregistry.io:5000/nginx:1.25`) is never mistaken for
/// a tag separator.
fn image_tag(image: &str) -> Option<&str> {
    if image.contains('@') {
        return None;
    }
    let repo_start = image.rfind('/').map(|i| i + 1).unwrap_or(0);
    let tail = &image[repo_start..];
    tail.rfind(':').map(|i| &tail[i + 1..])
}

/// `imagePullPolicy` (round 51; found in round 50's re-audit), including
/// real kubelet's own default-policy heuristic when unset: `Always` for
/// an untagged or `:latest`-tagged image (a floating reference that could
/// have changed since it was last pulled), `IfNotPresent` for anything
/// else (a specific version tag, or a digest — both immutable by
/// definition, so there's nothing to gain from re-checking the registry
/// every time).
fn effective_pull_policy<'a>(policy: Option<&'a str>, image: &str) -> &'a str {
    match policy {
        Some(p @ ("Always" | "IfNotPresent" | "Never")) => p,
        _ if image.contains('@') => "IfNotPresent", // digest-pinned: immutable, nothing to gain from re-checking
        _ => match image_tag(image) {
            None | Some("latest") => "Always",
            Some(_) => "IfNotPresent",
        },
    }
}

/// Real kubelet's `resourceFieldRef` output format (round 44; found in
/// round 35's re-audit): the raw value (millicores for CPU, bytes for
/// memory) is divided by the divisor (also in the same raw unit — a CPU
/// divisor is itself converted to millicores, a memory divisor to bytes),
/// then rounded **up** to a whole number of divisor-units and printed as a
/// plain integer — this is why an unset (default `"1"`) CPU divisor
/// famously reports whole cores, rounded up, rather than millicores. The
/// same formula handles both resource kinds identically; only the caller's
/// choice of raw unit and default divisor differs.
fn format_resource_field_value(raw: i64, divisor: i64) -> String {
    let divisor = divisor.max(1);
    ((raw + divisor - 1) / divisor).to_string()
}

/// Resolve one `resourceFieldRef` (env var or downwardAPI volume form) to
/// its plain-number string value. `limits.*` falls back to the node's own
/// capacity when the container has no such limit set — real kubelet's own
/// documented Downward API behavior (an unset limit is treated as "the
/// whole node," not zero/unbounded). `requests.*` falls back to the
/// container's own limit first (matching the general request-defaults-to-
/// limit rule), then to the node's capacity as the final fallback.
/// `ephemeral-storage` isn't tracked/enforced by nodelet at all (a
/// separate, pre-existing gap — see `docs/GAP_CLOSURE.md`), so it always
/// resolves to `"0"` rather than bailing.
fn resolve_resource_field_ref(
    reference: &k8s_openapi::api::core::v1::ResourceFieldSelector,
    resources: Option<&ResourceRequirements>,
    node_cpu_millicores: i64,
    node_memory_bytes: i64,
) -> Result<String> {
    let requests = resources.and_then(|r| r.requests.as_ref());
    let limits = resources.and_then(|r| r.limits.as_ref());
    let divisor_cpu = reference.divisor.as_ref().and_then(parse_cpu_millicores).filter(|d| *d > 0).unwrap_or(1000);
    let divisor_mem = reference.divisor.as_ref().and_then(parse_memory_bytes).filter(|d| *d > 0).unwrap_or(1);

    match reference.resource.as_str() {
        "limits.cpu" => {
            let m = limits.and_then(|r| r.get("cpu")).and_then(parse_cpu_millicores).unwrap_or(node_cpu_millicores);
            Ok(format_resource_field_value(m, divisor_cpu))
        }
        "requests.cpu" => {
            let m = requests
                .and_then(|r| r.get("cpu"))
                .and_then(parse_cpu_millicores)
                .or_else(|| limits.and_then(|r| r.get("cpu")).and_then(parse_cpu_millicores))
                .unwrap_or(node_cpu_millicores);
            Ok(format_resource_field_value(m, divisor_cpu))
        }
        "limits.memory" => {
            let b = limits.and_then(|r| r.get("memory")).and_then(parse_memory_bytes).unwrap_or(node_memory_bytes);
            Ok(format_resource_field_value(b, divisor_mem))
        }
        "requests.memory" => {
            let b = requests
                .and_then(|r| r.get("memory"))
                .and_then(parse_memory_bytes)
                .or_else(|| limits.and_then(|r| r.get("memory")).and_then(parse_memory_bytes))
                .unwrap_or(node_memory_bytes);
            Ok(format_resource_field_value(b, divisor_mem))
        }
        "limits.ephemeral-storage" | "requests.ephemeral-storage" => Ok("0".to_string()),
        other => bail!("resourceFieldRef: unsupported resource {other:?}"),
    }
}

/// kubelet's cpu.shares formula: `max(2, milliCPU * 1024 / 1000)`. No
/// request/limit at all still gets the cgroup-default minimum (2), same as
/// a real BestEffort pod.
fn cpu_shares_for(cpu_millicores: Option<i64>) -> i64 {
    match cpu_millicores {
        Some(m) if m > 0 => ((m * 1024) / 1000).max(2),
        _ => 2,
    }
}

/// What `ensure_container()` should do about an already-running container
/// whose live resources no longer match its (possibly just-edited) pod
/// spec — the in-place pod vertical scaling decision (round 42; found in
/// round 39's re-audit). Pulled out pure, same reasoning as every other
/// `*_decision()` function in this file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResizeDecision {
    /// Live resources already match the pod spec — nothing to do.
    NoChange,
    /// A changed resource's `resizePolicy` allows applying it without a
    /// restart (or none was specified — `NotRequired` is the real default).
    UpdateInPlace,
    /// A changed resource's `resizePolicy` explicitly requires
    /// `RestartContainer` — the caller should recreate the container
    /// exactly like `RestartDecision::NeedsRestart` already does.
    RequiresRestart,
}

/// Compare the pod spec's *desired* resources (freshly computed via
/// `linux_resources()`) against the *actual* resources last recorded for
/// this container (`CriRuntime::container_resources`, already tracked for
/// CPU Manager's shared-pool refresh — round 16). Deliberately only
/// compares the pod-spec-derived fields (`cpu_shares`/`cpu_quota`/
/// `cpu_period`/`memory_limit_in_bytes`), never `cpuset_cpus`/`cpuset_mems`
/// — those are owned independently by CPU/Memory Manager and can change
/// for reasons that have nothing to do with a spec edit (a neighboring
/// container's exclusive claim coming or going), which must never itself
/// be mistaken for a resize request.
fn resize_decision(
    desired: &LinuxContainerResources,
    actual: &LinuxContainerResources,
    resize_policies: Option<&[ContainerResizePolicy]>,
) -> ResizeDecision {
    let cpu_changed =
        desired.cpu_shares != actual.cpu_shares || desired.cpu_quota != actual.cpu_quota || desired.cpu_period != actual.cpu_period;
    let memory_changed = desired.memory_limit_in_bytes != actual.memory_limit_in_bytes;
    if !cpu_changed && !memory_changed {
        return ResizeDecision::NoChange;
    }
    let restart_required_for = |resource_name: &str| -> bool {
        resize_policies
            .unwrap_or(&[])
            .iter()
            .find(|p| p.resource_name == resource_name)
            .map(|p| p.restart_policy == "RestartContainer")
            .unwrap_or(false) // unspecified defaults to NotRequired, matching the API's own documented default
    };
    if (cpu_changed && restart_required_for("cpu")) || (memory_changed && restart_required_for("memory")) {
        ResizeDecision::RequiresRestart
    } else {
        ResizeDecision::UpdateInPlace
    }
}

/// Translate a container's `resources` into CRI's `LinuxContainerResources`.
/// CPU shares come from requests (falling back to limits if there's no
/// request, matching kubelet); CPU quota/period and the memory limit come
/// from limits only — a limit-less resource is left at CRI's "unspecified"
/// zero value, which containerd/runc treat as unconstrained.
/// `qos`/`node_memory_bytes` (round 28) only feed `oom_score_adj` — see
/// `eviction::oom_score_adj()`. Real kubelet computes this per container
/// (not per pod), using each container's own memory *request*, which is
/// why it's threaded through here rather than computed once per pod.
fn linux_resources(resources: Option<&ResourceRequirements>, qos: QosClass, node_memory_bytes: i64) -> LinuxContainerResources {
    let requests = resources.and_then(|r| r.requests.as_ref());
    let limits = resources.and_then(|r| r.limits.as_ref());
    let cpu_request = requests.and_then(|m| m.get("cpu")).and_then(parse_cpu_millicores);
    let cpu_limit = limits.and_then(|m| m.get("cpu")).and_then(parse_cpu_millicores);
    let mem_limit = limits.and_then(|m| m.get("memory")).and_then(parse_memory_bytes);
    let mem_request = requests.and_then(|m| m.get("memory")).and_then(parse_memory_bytes).unwrap_or(0);

    let (cpu_quota, cpu_period) = match cpu_limit {
        Some(m) if m > 0 => (CPU_CFS_QUOTA_PERIOD_US * m / 1000, CPU_CFS_QUOTA_PERIOD_US),
        _ => (0, 0),
    };

    LinuxContainerResources {
        cpu_shares: cpu_shares_for(cpu_request.or(cpu_limit)),
        cpu_quota,
        cpu_period,
        memory_limit_in_bytes: mem_limit.unwrap_or(0),
        oom_score_adj: crate::eviction::oom_score_adj(qos, mem_request, node_memory_bytes),
        hugepage_limits: hugepage_limits(limits),
        ..Default::default()
    }
}

/// A k8s hugepage resource name's binary-unit suffix (`"Mi"`/`"Gi"`/`"Ki"`)
/// -> CRI's own `HugepageLimit.page_size` format (round 59; found in
/// round 58's re-audit) — `"<size><unit-prefix>B"` (e.g. `"2MB"`,
/// `"1GB"`), matching the corresponding `hugetlb.<pagesize>.limit_in_bytes`
/// cgroup file name exactly. Despite looking decimal, the proto's own doc
/// comment confirms these are still parsed base-1024 — this is purely a
/// naming-convention translation (drop the trailing `i`, append `B`), not
/// a unit conversion; the byte *value* itself needs no rescaling.
fn hugepage_cri_page_size(k8s_suffix: &str) -> Option<String> {
    k8s_suffix.strip_suffix('i').map(|s| format!("{s}B"))
}

/// Every `resources.limits["hugepages-<size>"]` entry -> CRI's
/// `HugepageLimit` list, which has direct native support for exactly this
/// (`LinuxContainerResources.hugepage_limits`) — no host-side mount or
/// separate mechanism needed, unlike `emptyDir`'s own (still separately
/// tracked, still open) HugePages volume medium.
fn hugepage_limits(limits: Option<&BTreeMap<String, Quantity>>) -> Vec<v1::HugepageLimit> {
    let Some(limits) = limits else { return Vec::new() };
    limits
        .iter()
        .filter_map(|(name, q)| {
            let suffix = name.strip_prefix("hugepages-")?;
            let page_size = hugepage_cri_page_size(suffix)?;
            let bytes = parse_memory_bytes(q)?;
            Some(v1::HugepageLimit { page_size, limit: bytes.max(0) as u64 })
        })
        .collect()
}

/// Translate `spec.overhead` (a flat `ResourceList`, not a request/limit
/// pair) into `LinuxContainerResources` for `LinuxPodSandboxConfig.overhead`
/// — the per-sandbox cost a `RuntimeClass` declares on top of its
/// containers' own resources (e.g. gVisor's userspace kernel). Treated the
/// same as a limit for CPU-quota/memory-limit purposes, since overhead is
/// an amount to reserve/cap against, not something with its own
/// request/limit distinction.
fn resource_list_to_linux_resources(list: &BTreeMap<String, Quantity>) -> LinuxContainerResources {
    let cpu_millicores = list.get("cpu").and_then(parse_cpu_millicores);
    let mem_bytes = list.get("memory").and_then(parse_memory_bytes);

    let (cpu_quota, cpu_period) = match cpu_millicores {
        Some(m) if m > 0 => (CPU_CFS_QUOTA_PERIOD_US * m / 1000, CPU_CFS_QUOTA_PERIOD_US),
        _ => (0, 0),
    };

    LinuxContainerResources {
        cpu_shares: cpu_shares_for(cpu_millicores),
        cpu_quota,
        cpu_period,
        memory_limit_in_bytes: mem_bytes.unwrap_or(0),
        ..Default::default()
    }
}

/// Translate pod- and container-level `securityContext` into CRI's
/// `LinuxContainerSecurityContext`. Container-level fields override pod-level
/// ones wherever Kubernetes defines both (matches real kubelet semantics).
/// Not translated yet (see docs/GAP_CLOSURE.md): AppArmor profile, SELinux
/// options, and runAsNonRoot *verification* against the image's actual user
/// (that needs image inspection, not just pass-through).
fn linux_security_context(
    pod_sc: Option<&PodSecurityContext>,
    container_sc: Option<&SecurityContext>,
    pid_mode: NamespaceMode,
) -> LinuxContainerSecurityContext {
    let run_as_user = container_sc
        .and_then(|s| s.run_as_user)
        .or_else(|| pod_sc.and_then(|s| s.run_as_user));
    let run_as_group = container_sc
        .and_then(|s| s.run_as_group)
        .or_else(|| pod_sc.and_then(|s| s.run_as_group));
    let privileged = container_sc.and_then(|s| s.privileged).unwrap_or(false);
    let readonly_rootfs = container_sc.and_then(|s| s.read_only_root_filesystem).unwrap_or(false);
    let no_new_privs = container_sc.and_then(|s| s.allow_privilege_escalation) == Some(false);
    let capabilities = container_sc.and_then(|s| s.capabilities.as_ref()).map(|c| Capability {
        add_capabilities: c.add.clone().unwrap_or_default(),
        drop_capabilities: c.drop.clone().unwrap_or_default(),
        ..Default::default()
    });
    let supplemental_groups = pod_sc
        .and_then(|s| s.supplemental_groups.clone())
        .unwrap_or_default();
    let seccomp = seccomp_profile(pod_sc, container_sc);

    LinuxContainerSecurityContext {
        run_as_user: run_as_user.map(|value| Int64Value { value }),
        run_as_group: run_as_group.map(|value| Int64Value { value }),
        privileged,
        readonly_rootfs,
        no_new_privs,
        capabilities,
        supplemental_groups,
        seccomp,
        namespace_options: Some(NamespaceOption { pid: pid_mode as i32, ..Default::default() }),
        ..Default::default()
    }
}

/// Container-level `seccompProfile` wins over the pod-level one, matching
/// Kubernetes' own override rule. `None` (neither set) means "let the
/// runtime pick its own default" — leaving CRI's `seccomp` field unset,
/// same as before this existed.
fn seccomp_profile(
    pod_sc: Option<&PodSecurityContext>,
    container_sc: Option<&SecurityContext>,
) -> Option<SecurityProfile> {
    let profile = container_sc
        .and_then(|s| s.seccomp_profile.as_ref())
        .or_else(|| pod_sc.and_then(|s| s.seccomp_profile.as_ref()))?;
    Some(match profile.type_.as_str() {
        "RuntimeDefault" => SecurityProfile { profile_type: ProfileType::RuntimeDefault as i32, ..Default::default() },
        "Localhost" => SecurityProfile {
            profile_type: ProfileType::Localhost as i32,
            localhost_ref: profile.localhost_profile.clone().unwrap_or_default(),
        },
        _ => SecurityProfile { profile_type: ProfileType::Unconfined as i32, ..Default::default() },
    })
}

/// Build the CRI `DnsConfig` for a pod, honoring `dnsPolicy` +
/// custom `dnsConfig`. `dnsPolicy: Default` means "inherit the node's own
/// resolv.conf" — returning `None` leaves containerd's own default in place,
/// which is exactly that. `ClusterFirst` (the pod-spec default) only takes
/// effect if the node was actually configured with cluster DNS servers
/// (`NODELET_CLUSTER_DNS`); an edge device with no cluster DNS server falls
/// back to the host's resolv.conf rather than pointing pods at nothing.
fn dns_config_for(pod: &Pod, cluster_dns: &[String], cluster_domain: &str) -> Option<DnsConfig> {
    let policy = pod
        .spec
        .as_ref()
        .and_then(|s| s.dns_policy.clone())
        .unwrap_or_else(|| "ClusterFirst".to_string());

    let mut servers = Vec::new();
    let mut searches = Vec::new();
    let mut options = Vec::new();

    if matches!(policy.as_str(), "ClusterFirst" | "ClusterFirstWithHostNet") && !cluster_dns.is_empty() {
        servers = cluster_dns.to_vec();
        let ns = pod.metadata.namespace.as_deref().unwrap_or("default");
        searches = vec![
            format!("{ns}.svc.{cluster_domain}"),
            format!("svc.{cluster_domain}"),
            cluster_domain.to_string(),
        ];
        options = vec!["ndots:5".to_string()];
    } else if policy == "Default" {
        return None; // explicit "use the host's own resolv.conf" — nothing to set
    }

    if let Some(dns_config) = pod.spec.as_ref().and_then(|s| s.dns_config.as_ref()) {
        servers.extend(dns_config.nameservers.clone().unwrap_or_default());
        searches.extend(dns_config.searches.clone().unwrap_or_default());
        options.extend(dns_config.options.clone().unwrap_or_default().into_iter().filter_map(|o| {
            let name = o.name?;
            Some(match o.value {
                Some(v) => format!("{name}:{v}"),
                None => name,
            })
        }));
    }

    if servers.is_empty() && searches.is_empty() && options.is_empty() {
        None
    } else {
        Some(DnsConfig { servers, searches, options })
    }
}

/// Real kubelet's `GeneratePodHostNameAndDomain` + `ShouldSetHostnameAsFQDN`
/// logic (round 38; found in round 35's re-audit): `spec.hostname` overrides
/// the sandbox hostname (defaults to the pod name); `spec.subdomain`
/// combines with the namespace/cluster domain to form the pod's headless-
/// Service search domain; `setHostnameAsFQDN` (only meaningful when
/// `subdomain` is also set) makes the *hostname itself* the full FQDN
/// instead of just the short name. Linux's `sethostname(2)` rejects
/// anything over `HOST_NAME_MAX` (64 bytes) — real kubelet fails the pod
/// rather than silently truncating, so this does too (via `Err`, which
/// `ensure_pod()`'s existing retry-and-report-failure path already handles,
/// no new failure mechanism needed).
fn resolve_pod_hostname(
    hostname: Option<&str>,
    subdomain: Option<&str>,
    set_hostname_as_fqdn: bool,
    pod_name: &str,
    namespace: &str,
    cluster_domain: &str,
) -> Result<String> {
    let short = hostname.unwrap_or(pod_name);
    let Some(subdomain) = subdomain.filter(|s| !s.is_empty()) else {
        return Ok(short.to_string());
    };
    if !set_hostname_as_fqdn {
        return Ok(short.to_string());
    }
    let fqdn = format!("{short}.{subdomain}.{namespace}.svc.{cluster_domain}");
    if fqdn.len() > 64 {
        bail!("setHostnameAsFQDN: FQDN '{fqdn}' is {} bytes, longer than the 64-byte Linux hostname limit", fqdn.len());
    }
    Ok(fqdn)
}

/// `spec.securityContext.sysctls` -> CRI's `LinuxPodSandboxConfig.sysctls`
/// map (round 41; found in round 39's re-audit). A later duplicate `name`
/// in the list simply overwrites an earlier one in the resulting map —
/// the apiserver's own validation already rejects duplicate sysctl names
/// within a single Pod, so this never has to arbitrate a real conflict.
fn pod_sysctls(pod_sc: Option<&PodSecurityContext>) -> HashMap<String, String> {
    pod_sc
        .and_then(|sc| sc.sysctls.as_ref())
        .map(|list| list.iter().map(|s| (s.name.clone(), s.value.clone())).collect())
        .unwrap_or_default()
}

/// Return the Kubernetes VolumeSource variant for diagnostics. A volume's
/// name alone is not enough to explain why it was skipped — in particular,
/// kube-controller-manager injects a volume named `kube-api-access-*` whose
/// source is `projected`, not `hostPath` or `emptyDir`.
fn volume_source_type(v: &Volume) -> &'static str {
    if v.config_map.is_some() {
        "configMap"
    } else if v.secret.is_some() {
        "secret"
    } else if v.empty_dir.is_some() {
        "emptyDir"
    } else if v.projected.is_some() {
        "projected"
    } else if v.host_path.is_some() {
        "hostPath"
    } else if v.downward_api.is_some() {
        "downwardAPI"
    } else if v.persistent_volume_claim.is_some() {
        "persistentVolumeClaim"
    } else if v.csi.is_some() {
        "csi"
    } else if v.ephemeral.is_some() {
        "ephemeral"
    } else if v.nfs.is_some() {
        "nfs"
    } else if v.aws_elastic_block_store.is_some() {
        "awsElasticBlockStore"
    } else if v.azure_disk.is_some() {
        "azureDisk"
    } else if v.azure_file.is_some() {
        "azureFile"
    } else if v.cephfs.is_some() {
        "cephfs"
    } else if v.cinder.is_some() {
        "cinder"
    } else if v.fc.is_some() {
        "fc"
    } else if v.flex_volume.is_some() {
        "flexVolume"
    } else if v.flocker.is_some() {
        "flocker"
    } else if v.gce_persistent_disk.is_some() {
        "gcePersistentDisk"
    } else if v.git_repo.is_some() {
        "gitRepo"
    } else if v.glusterfs.is_some() {
        "glusterfs"
    } else if v.iscsi.is_some() {
        "iscsi"
    } else if v.photon_persistent_disk.is_some() {
        "photonPersistentDisk"
    } else if v.portworx_volume.is_some() {
        "portworx"
    } else if v.quobyte.is_some() {
        "quobyte"
    } else if v.rbd.is_some() {
        "rbd"
    } else if v.scale_io.is_some() {
        "scaleIO"
    } else if v.storageos.is_some() {
        "storageos"
    } else if v.vsphere_volume.is_some() {
        "vsphereVolume"
    } else {
        "unknown"
    }
}

/// Convert a Service or port name to the form used by Kubernetes' legacy
/// service-environment mechanism (`my-api` -> `MY_API`).
fn env_name_component(name: &str) -> String {
    name.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_uppercase() } else { '_' })
        .collect()
}

/// Add the Service discovery variables kubelet normally injects into every
/// container. In particular, client-go's in-cluster configuration requires
/// `KUBERNETES_SERVICE_HOST` and `KUBERNETES_SERVICE_PORT`.
///
/// Services in the Pod's namespace are included, as are Services in the
/// default namespace. The latter makes the cluster's `kubernetes` Service
/// discoverable from system namespaces such as `kube-system`.
fn service_env_vars(services: &[Service], pod_namespace: &str) -> BTreeMap<String, Vec<u8>> {
    let mut values = BTreeMap::new();

    // Add default-namespace Services first so a same-named Service in the
    // Pod's own namespace takes precedence below.
    for service in services.iter().filter(|s| {
        s.metadata.namespace.as_deref().unwrap_or("default") == "default" && pod_namespace != "default"
    }) {
        add_service_env_vars(&mut values, service);
    }
    for service in services.iter().filter(|s| {
        s.metadata.namespace.as_deref().unwrap_or("default") == pod_namespace
    }) {
        add_service_env_vars(&mut values, service);
    }

    values
}

fn add_service_env_vars(values: &mut BTreeMap<String, Vec<u8>>, service: &Service) {
    let Some(name) = service.metadata.name.as_deref().filter(|n| !n.is_empty()) else {
        return;
    };
    let Some(spec) = service.spec.as_ref() else { return };
    if spec.type_.as_deref() == Some("ExternalName") {
        return;
    }
    let Some(cluster_ip) = spec.cluster_ip.as_deref().filter(|ip| !ip.is_empty() && *ip != "None") else {
        return;
    };

    let prefix = env_name_component(name);
    put_env(values, format!("{prefix}_SERVICE_HOST"), cluster_ip.as_bytes().to_vec());

    for (index, port) in spec.ports.as_deref().unwrap_or(&[]).iter().enumerate() {
        let port_value = port.port.to_string();
        if index == 0 {
            put_env(values, format!("{prefix}_SERVICE_PORT"), port_value.as_bytes().to_vec());
        }
        if let Some(port_name) = port.name.as_deref().filter(|n| !n.is_empty()) {
            put_env(
                values,
                format!("{prefix}_SERVICE_PORT_{}", env_name_component(port_name)),
                port_value.as_bytes().to_vec(),
            );
        }

        // The legacy *_PORT_* variables are still emitted by kubelet and are
        // used by some images even when *_SERVICE_* is not.
        let protocol = port.protocol.as_deref().unwrap_or("TCP");
        let protocol_lower = protocol.to_ascii_lowercase();
        let protocol_upper = protocol.to_ascii_uppercase();
        let uri_host = if cluster_ip.contains(':') {
            format!("[{cluster_ip}]")
        } else {
            cluster_ip.to_string()
        };
        let uri = format!("{protocol_lower}://{uri_host}:{port_value}");
        put_env(values, format!("{prefix}_PORT"), uri.as_bytes().to_vec());
        let port_prefix = format!("{prefix}_PORT_{}_{}", port.port, protocol_upper);
        put_env(values, port_prefix.clone(), uri.as_bytes().to_vec());
        put_env(values, format!("{port_prefix}_PROTO"), protocol_lower.as_bytes().to_vec());
        put_env(values, format!("{port_prefix}_PORT"), port_value.as_bytes().to_vec());
        put_env(values, format!("{port_prefix}_ADDR"), cluster_ip.as_bytes().to_vec());
    }
}

fn put_env(values: &mut BTreeMap<String, Vec<u8>>, key: String, value: Vec<u8>) {
    values.insert(key, value);
}

/// Resolve a downward-API fieldRef from the Pod object supplied by the API
/// server.
fn pod_field_value(pod: &Pod, field_path: &str) -> Option<String> {
    match field_path {
        "metadata.name" => pod.metadata.name.clone(),
        "metadata.namespace" => pod.metadata.namespace.clone(),
        "metadata.uid" => pod.metadata.uid.clone(),
        "spec.nodeName" => pod.spec.as_ref()?.node_name.clone(),
        "spec.serviceAccountName" => Some(
            pod.spec
                .as_ref()?
                .service_account_name
                .clone()
                .unwrap_or_else(|| "default".to_string()),
        ),
        "status.hostIP" => pod.status.as_ref()?.host_ip.clone(),
        "status.podIP" => pod.status.as_ref()?.pod_ip.clone(),
        "status.podIPs" => pod
            .status
            .as_ref()?
            .pod_ips
            .as_ref()?
            .first()
            .map(|ip| ip.ip.clone()),
        _ => {
            if let Some(key) = field_path
                .strip_prefix("metadata.labels[")
                .and_then(|s| s.strip_suffix(']'))
            {
                return pod
                    .metadata
                    .labels
                    .as_ref()?
                    .get(key.trim_matches(|c| c == '\'' || c == '"'))
                    .cloned();
            }
            if let Some(key) = field_path
                .strip_prefix("metadata.annotations[")
                .and_then(|s| s.strip_suffix(']'))
            {
                return pod
                    .metadata
                    .annotations
                    .as_ref()?
                    .get(key.trim_matches(|c| c == '\'' || c == '"'))
                    .cloned();
            }
            None
        }
    }
}

/// The deterministic name a generic ephemeral volume's (`spec.volumes[].ephemeral`)
/// auto-created `PersistentVolumeClaim` gets — `<pod name>-<volume name>`,
/// exactly as documented on `EphemeralVolumeSource` itself (round 31).
/// Pure so the naming convention is unit-testable without a cluster.
fn ephemeral_pvc_name(pod_name: &str, volume_name: &str) -> String {
    format!("{pod_name}-{volume_name}")
}

/// Synthetic `volume_id` for a CSI *ephemeral inline* volume (round 46) —
/// there's no PV/PVC to derive a real one from, so nodelet mints its own,
/// scoped by pod UID (stable across reconciles, unique across pod
/// recreations even under the same name) and volume name (unique within
/// one pod).
fn csi_ephemeral_volume_handle(pod_uid: &str, volume_name: &str) -> String {
    format!("{pod_uid}-{volume_name}")
}

/// Whether `pvc` is genuinely owned by the pod with uid `pod_uid` — the
/// safety check real kubelet itself does before trusting a same-named
/// PVC for a generic ephemeral volume (round 31; see
/// `EphemeralVolumeSource`'s own doc comment: "An existing PVC with that
/// name that is not owned by the pod will *not* be used ... to avoid
/// using an unrelated volume by mistake"). Checked by UID, not just
/// name/kind — a stale or coincidentally-named PVC must never be
/// silently adopted.
fn pvc_owned_by_pod(pvc: &PersistentVolumeClaim, pod_uid: &str) -> bool {
    pvc.metadata.owner_references.as_deref().unwrap_or(&[]).iter().any(|o| o.uid == pod_uid)
}

/// Whether an `emptyDir` volume wants `medium: Memory` (tmpfs-backed,
/// round 30) rather than the default (unset, or explicitly `""`) —
/// regular disk. Pure so the decision is unit-testable without touching
/// the filesystem.
fn is_memory_medium_empty_dir(source: &k8s_openapi::api::core::v1::EmptyDirVolumeSource) -> bool {
    source.medium.as_deref() == Some("Memory")
}

/// Build `mount -t tmpfs [-o size=<bytes>] tmpfs <path>`'s arguments —
/// pure so the command construction is unit-testable without actually
/// mounting anything. No `sizeLimit` set means no `-o size=`, matching
/// tmpfs's own kernel default (half of physical RAM) rather than nodelet
/// inventing a cap upstream doesn't itself impose in that case.
fn tmpfs_mount_args(path: &std::path::Path, size_limit_bytes: Option<i64>) -> Vec<String> {
    let mut args = vec!["-t".to_string(), "tmpfs".to_string()];
    if let Some(bytes) = size_limit_bytes.filter(|b| *b > 0) {
        args.push("-o".to_string());
        args.push(format!("size={bytes}"));
    }
    args.push("tmpfs".to_string());
    args.push(path.to_string_lossy().into_owned());
    args
}

/// Mount a `Memory`-medium `emptyDir` volume's directory as tmpfs — the
/// same approach real kubelet itself uses (kubelet mounts tmpfs directly
/// on the host path it hands the container runtime as a bind-mount
/// source; this isn't a CRI-level concept, CRI's `Mount` struct only
/// binds an *existing* host path, it doesn't control the filesystem type
/// backing it). Shells out to `mount(8)` — same "use the host's own
/// tools rather than raw syscalls" approach `svc.rs` already takes for
/// `nft`. Best-effort: a failure here is logged and the (already-created,
/// plain-disk) directory is used as a fallback rather than failing the
/// whole pod — the same graceful-degradation posture used everywhere
/// else a host-level operation might not be available (e.g. no root, no
/// tmpfs support at all).
fn mount_tmpfs_empty_dir(dir: &std::path::Path, size_limit_bytes: Option<i64>) -> Result<()> {
    let status = std::process::Command::new("mount")
        .args(tmpfs_mount_args(dir, size_limit_bytes))
        .status()
        .context("running mount(8)")?;
    if !status.success() {
        anyhow::bail!("mount -t tmpfs exited with {status}");
    }
    Ok(())
}

/// Unmount every `Memory`-medium `emptyDir` this pod declared — called on
/// pod teardown (`remove_pod()`) since a tmpfs mount is real RAM that
/// must be given back, unlike a plain-disk `emptyDir` directory (left in
/// place today regardless of medium — a pre-existing simplification, see
/// `docs/GAP_CLOSURE.md`). Re-derives volume names/paths from the Pod
/// object rather than tracking mount state separately, the same approach
/// `unmount_csi_volumes()` already takes. Best-effort per volume — one
/// already-gone mount (e.g. the pod directory was already cleaned up some
/// other way) must not stop the rest of teardown.
fn unmount_memory_backed_empty_dirs(pod: &Pod, id: &PodId) {
    let Some(volumes) = pod.spec.as_ref().and_then(|s| s.volumes.as_ref()) else { return };
    let pod_dir = PathBuf::from(VOLUME_ROOT).join(&id.uid).join("volumes");
    for v in volumes {
        let Some(source) = &v.empty_dir else { continue };
        if !is_memory_medium_empty_dir(source) {
            continue;
        }
        let vol_dir = pod_dir.join(&v.name);
        if let Err(e) = std::process::Command::new("umount").arg(&vol_dir).status() {
            warn!(volume = %v.name, path = %vol_dir.display(), error = ?e, "failed to run umount for a Memory-medium emptyDir volume");
        }
    }
}

/// Write a ConfigMap/Secret's keys out as individual files under `dir`
/// (creating it if needed) — text values from `.data`/`.stringData`, binary
/// values from `.binaryData`/`.data` (Secret's `.data` is base64 in the wire
/// format but k8s_openapi's `ByteString` decodes it automatically, so by the
/// time it gets here it's already raw bytes).
fn write_volume_dir(
    dir: &std::path::Path,
    text: Option<std::collections::BTreeMap<String, String>>,
    binary: Option<std::collections::BTreeMap<String, Vec<u8>>>,
) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    for (k, v) in text.into_iter().flatten() {
        std::fs::write(dir.join(k), v)?;
    }
    for (k, v) in binary.into_iter().flatten() {
        std::fs::write(dir.join(k), v)?;
    }
    Ok(())
}

/// Write a downwardAPI volume's files (`fieldRef` only — `resourceFieldRef`,
/// e.g. a container's actual assigned CPU/memory limit, isn't supported: it
/// needs the resolved container spec, not just the Pod object). An item's
/// `path` may contain subdirectories, which is valid Kubernetes downwardAPI
/// syntax.
fn write_downward_api_volume(
    dir: &std::path::Path,
    pod: &Pod,
    items: &[k8s_openapi::api::core::v1::DownwardAPIVolumeFile],
) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    for item in items {
        let Some(field_ref) = &item.field_ref else { continue };
        let Some(value) = pod_field_value(pod, &field_ref.field_path) else { continue };
        let target = dir.join(&item.path);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(target, value)?;
    }
    Ok(())
}

/// Write one projected-volume source's contribution into `dir`. Mirrors
/// `write_volume_dir()`'s "every key becomes a file" default, but a
/// projected source additionally supports `items` (`KeyToPath`): select
/// specific keys and rename them to a specific path within the volume —
/// real Kubernetes semantics that plain top-level configMap/secret volumes
/// also have but nodelet doesn't apply there yet (see docs/GAP_CLOSURE.md).
fn write_projected_keys(
    dir: &std::path::Path,
    text: Option<std::collections::BTreeMap<String, String>>,
    binary: Option<std::collections::BTreeMap<String, Vec<u8>>>,
    items: Option<&[k8s_openapi::api::core::v1::KeyToPath]>,
) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let write_one = |path: &str, bytes: &[u8]| -> std::io::Result<()> {
        let target = dir.join(path);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(target, bytes)
    };
    match items {
        Some(items) => {
            for item in items {
                if let Some(v) = text.as_ref().and_then(|m| m.get(&item.key)) {
                    write_one(&item.path, v.as_bytes())?;
                } else if let Some(v) = binary.as_ref().and_then(|m| m.get(&item.key)) {
                    write_one(&item.path, v)?;
                }
            }
        }
        None => {
            for (k, v) in text.into_iter().flatten() {
                write_one(&k, v.as_bytes())?;
            }
            for (k, v) in binary.into_iter().flatten() {
                write_one(&k, &v)?;
            }
        }
    }
    Ok(())
}

/// Build a pod's `/etc/hosts` contents from `hostAliases` — kubelet's own
/// approach: it doesn't tell the container runtime about extra hosts
/// entries (CRI has no such field), it generates the file itself and bind
/// mounts it over `/etc/hosts`.
fn write_etc_hosts(path: &std::path::Path, aliases: &[k8s_openapi::api::core::v1::HostAlias]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut content = String::from("127.0.0.1\tlocalhost\n::1\tlocalhost ip6-localhost ip6-loopback\n");
    for alias in aliases {
        let hostnames = alias.hostnames.clone().unwrap_or_default().join(" ");
        if !hostnames.is_empty() {
            content.push_str(&format!("{}\t{hostnames}\n", alias.ip));
        }
    }
    std::fs::write(path, content)
}

/// Set the group ownership of `path` without touching its user owner
/// (`(uid_t)-1` is POSIX for "leave unchanged").
fn chown_gid(path: &std::path::Path, gid: u32) -> std::io::Result<()> {
    use std::os::unix::ffi::OsStrExt;
    let c_path = std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
    let rc = unsafe { libc::chown(c_path.as_ptr(), u32::MAX as libc::uid_t, gid as libc::gid_t) };
    if rc != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// Set the setgid bit on a directory so files later written into it by the
/// container process inherit `fsGroup` too, matching real kubelet's
/// volume-ownership behavior.
fn set_setgid(dir: &std::path::Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let meta = std::fs::metadata(dir)?;
    let mut perm = meta.permissions();
    perm.set_mode(perm.mode() | 0o2000);
    std::fs::set_permissions(dir, perm)
}

/// Recursively chown a materialized volume directory to `fsGroup`. Only
/// touches directories nodelet itself created (ConfigMap/Secret/emptyDir/
/// downwardAPI/projected materializations) — there's no real PV/hostPath
/// support yet for this to reach beyond that (see docs/GAP_CLOSURE.md).
fn apply_fs_group(dir: &std::path::Path, gid: u32) -> std::io::Result<()> {
    chown_gid(dir, gid)?;
    set_setgid(dir)?;
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_dir() {
            apply_fs_group(&path, gid)?;
        } else {
            chown_gid(&path, gid)?;
        }
    }
    Ok(())
}

/// Shift `path`, `path.1`, `path.2`, ... up by one, dropping whatever would
/// fall off the end past `max_files`, then leave `path` itself absent — the
/// caller (CRI's `ReopenContainerLog`) is what makes the runtime create a
/// fresh one. `max_files` counts the active file too, matching kubelet's
/// `--container-log-max-files` semantics (default 5: the current log plus
/// 4 rotated ones).
fn rotate_log_file(path: &std::path::Path, max_files: u32) -> std::io::Result<()> {
    let rotated = |n: u32| std::path::PathBuf::from(format!("{}.{n}", path.display()));
    let max_files = max_files.max(1);
    if max_files == 1 {
        // No rotated copies kept at all — just drop the oversized log.
        return std::fs::remove_file(path);
    }

    // Oldest surviving slot first: drop anything that would land past max_files.
    for n in (1..max_files).rev() {
        let from = rotated(n);
        if !from.exists() {
            continue;
        }
        if n + 1 >= max_files {
            std::fs::remove_file(&from)?;
        } else {
            std::fs::rename(&from, rotated(n + 1))?;
        }
    }
    std::fs::rename(path, rotated(1))?;
    Ok(())
}

/// The ServiceAccount a Pod runs as — `default` when unset, matching real
/// Kubernetes (every namespace has an auto-created `default` ServiceAccount).
fn pod_service_account_name(pod: &Pod) -> String {
    pod.spec
        .as_ref()
        .and_then(|s| s.service_account_name.clone())
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| "default".to_string())
}

/// `ServiceAccountTokenProjection.audience` is a single optional string;
/// `TokenRequestSpec.audiences` wants a `Vec` (empty meaning "apiserver's
/// own default audience").
fn token_audiences(audience: Option<&str>) -> Vec<String> {
    audience.filter(|a| !a.is_empty()).map(|a| vec![a.to_string()]).unwrap_or_default()
}

/// The registry host an image reference pulls from, e.g. `myregistry.io:5000`
/// from `myregistry.io:5000/team/app:v1`, or `docker.io` for an unqualified
/// ref like `busybox:latest` (Docker Hub's implicit default registry).
fn registry_host_for_image(image: &str) -> String {
    // A single-segment ref (no '/' at all) is always an official Docker Hub
    // image ("busybox:latest") — its ':' is the tag separator, not a host
    // port, so it must never reach the "looks like a host" check below.
    let Some((first_segment, _rest)) = image.split_once('/') else {
        return "docker.io".to_string();
    };
    let looks_like_a_host = first_segment.contains('.') || first_segment.contains(':') || first_segment == "localhost";
    if looks_like_a_host {
        first_segment.to_string()
    } else {
        "docker.io".to_string()
    }
}

/// Extract `{username, password}` for `registry_host` out of a
/// `kubernetes.io/dockerconfigjson` Secret's `.dockerconfigjson` bytes
/// (`{"auths": {"<host>": {"username","password"} | {"auth": base64(u:p)}}}`).
/// Legacy `kubernetes.io/dockercfg` (no `"auths"` wrapper) isn't handled —
/// dockerconfigjson is what every current `kubectl create secret
/// docker-registry` produces.
fn parse_dockerconfigjson(bytes: &[u8], registry_host: &str) -> Option<(String, String)> {
    let json: serde_json::Value = serde_json::from_slice(bytes).ok()?;
    let auths = json.get("auths")?.as_object()?;

    // Docker Hub is recorded under several historical aliases.
    let candidates: Vec<&str> = if registry_host == "docker.io" {
        vec!["docker.io", "https://index.docker.io/v1/", "index.docker.io"]
    } else {
        vec![registry_host]
    };

    for key in candidates {
        let Some(entry) = auths.get(key) else { continue };
        if let (Some(u), Some(p)) =
            (entry.get("username").and_then(|v| v.as_str()), entry.get("password").and_then(|v| v.as_str()))
        {
            return Some((u.to_string(), p.to_string()));
        }
        if let Some(encoded) = entry.get("auth").and_then(|v| v.as_str()) {
            use base64::Engine;
            let decoded = base64::engine::general_purpose::STANDARD.decode(encoded).ok()?;
            let decoded = String::from_utf8(decoded).ok()?;
            let (u, p) = decoded.split_once(':')?;
            return Some((u.to_string(), p.to_string()));
        }
    }
    None
}

/// Fire a bare-minimum HTTP/1.1 GET for a `postStart`/`preStop` `httpGet`
/// lifecycle hook. Result is deliberately not inspected by the caller —
/// matches real kubelet, which only logs a failed lifecycle httpGet rather
/// than acting on it.
async fn lifecycle_http_get(host: &str, port: u16, path: &str) {
    if port == 0 {
        return;
    }
    let Ok(mut stream) = TcpStream::connect((host, port)).await else { return };
    let req = format!("GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n");
    let _ = stream.write_all(req.as_bytes()).await;
    let mut buf = [0u8; 256];
    let _ = stream.read(&mut buf).await;
}

/// Dial a unix-domain CRI socket (e.g. `unix:///run/containerd/containerd.sock`).
async fn connect_uds(endpoint: &str) -> Result<Channel> {
    let path = endpoint
        .strip_prefix("unix://")
        .unwrap_or(endpoint)
        .to_string();
    // The URI is a placeholder; the custom connector ignores it and dials the socket.
    let channel = Endpoint::try_from("http://localhost")
        .context("invalid endpoint")?
        .connect_with_connector(tower::service_fn(move |_: Uri| {
            let path = path.clone();
            async move {
                let stream = tokio::net::UnixStream::connect(path).await?;
                Ok::<_, std::io::Error>(hyper_util::rt::TokioIo::new(stream))
            }
        }))
        .await
        .context("connecting to CRI unix socket")?;
    Ok(channel)
}

impl CriRuntime {
    pub async fn connect(
        endpoint: &str,
        client: kube::Client,
        node_name: String,
        cluster_dns: Vec<String>,
        cluster_domain: String,
        csi_drivers: BTreeMap<String, String>,
        plugin_registry_path: String,
        plugin_registry_sync_interval: Duration,
        cpu_manager: Option<crate::cpu_manager::CpuManager>,
        memory_manager: Option<crate::memory_manager::MemoryManager>,
        topology_policy: crate::topology::TopologyManagerPolicy,
        numa_topology: BTreeMap<u32, BTreeSet<u32>>,
        userns: crate::userns::UsernsAllocator,
        node_memory_bytes: i64,
        node_cpu_millicores: i64,
    ) -> Result<Self> {
        let channel = connect_uds(endpoint).await?;
        let rt = RuntimeServiceClient::new(channel.clone());
        let img = ImageServiceClient::new(channel.clone());

        // This runtime's own name (round 57), for the `<runtimeName>://<id>`
        // prefix real kubelet always puts on container IDs in status.
        // Best-effort: a failure here shouldn't block startup over what's
        // ultimately a cosmetic formatting detail.
        let mut version_client = rt.clone();
        let runtime_name = match version_client.version(VersionRequest { version: String::new() }).await {
            Ok(resp) => resp.into_inner().runtime_name,
            Err(e) => {
                warn!(error = ?e, "CRI Version call failed; container IDs in status will use a generic runtime name");
                "unknown".to_string()
            }
        };

        // Spawn the event subscriber (event-driven status, no polling).
        let (tx, rx) = unbounded_channel();
        tokio::spawn(event_loop(channel, tx));

        let csi = Arc::new(crate::runtime::csi::CsiDrivers::new(csi_drivers));
        let device_plugins = Arc::new(crate::device_plugins::DevicePlugins::new());
        // Dynamic CSI driver / device plugin discovery: watches
        // plugin_registry_path for a plugin's registrar socket, same
        // protocol real kubelet's own plugin watcher speaks (see
        // plugin_registry.rs) — a no-op loop if nothing ever registers there.
        tokio::spawn(crate::plugin_registry::run(csi.clone(), device_plugins.clone(), plugin_registry_path, plugin_registry_sync_interval));

        Ok(Self {
            rt,
            img,
            client,
            node_name,
            cluster_dns,
            cluster_domain,
            rx: Mutex::new(Some(rx)),
            restart_policies: Mutex::new(HashMap::new()),
            pod_uids: Mutex::new(HashMap::new()),
            sidecar_names: Mutex::new(HashMap::new()),
            userns,
            node_memory_bytes,
            node_cpu_millicores,
            runtime_name,
            restart_counts: Mutex::new(HashMap::new()),
            csi,
            device_plugins,
            device_allocations: Mutex::new(HashMap::new()),
            cpu_manager,
            memory_manager,
            container_resources: Mutex::new(HashMap::new()),
            applied_resources: Mutex::new(HashMap::new()),
            spec_resources: Mutex::new(HashMap::new()),
            topology_policy,
            numa_topology,
        })
    }

    /// Materialize every ConfigMap/Secret/emptyDir volume this Pod declares
    /// onto the host filesystem, and return volume name -> host directory.
    /// ConfigMap/Secret keys become individual files inside that directory
    /// (matching how a real kubelet lays them out, and how a Corefile-style
    /// single-key mount ends up as e.g. `.../Corefile`). Volume kinds this
    /// doesn't understand yet (projected/serviceAccountToken, hostPath,
    /// downwardAPI, ...) are skipped with a warning rather than silently
    /// producing an empty mount — a container that needs one of those still
    /// won't get it, but at least it's visible in the logs why, instead of
    /// looking identical to the ConfigMap bug this fixes.
    async fn resolve_volumes(&self, pod: &Pod, id: &PodId, pull_secrets: &[String]) -> HashMap<String, ResolvedVolume> {
        let mut out = HashMap::new();
        let Some(volumes) = pod.spec.as_ref().and_then(|s| s.volumes.as_ref()) else {
            return out;
        };
        let pod_dir = PathBuf::from(VOLUME_ROOT).join(&id.uid).join("volumes");

        for v in volumes {
            let vol_dir = pod_dir.join(&v.name);

            if let Some(cm) = &v.config_map {
                let name = &cm.name;
                let optional = cm.optional.unwrap_or(false);
                match Api::<ConfigMap>::namespaced(self.client.clone(), &id.namespace).get(name).await {
                    Ok(obj) => {
                        if let Err(e) = write_volume_dir(&vol_dir, obj.data, obj.binary_data.map(|m| {
                            m.into_iter().map(|(k, v)| (k, v.0)).collect()
                        })) {
                            warn!(volume = %v.name, configmap = %name, error = ?e, "failed to materialize ConfigMap volume");
                            continue;
                        }
                        out.insert(v.name.clone(), ResolvedVolume::HostPath(vol_dir));
                    }
                    // A missing ConfigMap on a volume explicitly marked
                    // `optional: true` (coredns's own manifest does this for
                    // its "coredns-custom" volume, for exactly this reason —
                    // it's fine for that ConfigMap to not exist) isn't a
                    // real problem; only warn for a genuinely required one.
                    Err(_) if optional => {}
                    Err(e) => warn!(volume = %v.name, configmap = %name, error = ?e, "failed to fetch ConfigMap for volume"),
                }
            } else if let Some(sec) = &v.secret {
                let Some(name) = &sec.secret_name else { continue };
                let optional = sec.optional.unwrap_or(false);
                match Api::<Secret>::namespaced(self.client.clone(), &id.namespace).get(name).await {
                    Ok(obj) => {
                        let bin = obj.data.map(|m| m.into_iter().map(|(k, v)| (k, v.0)).collect());
                        if let Err(e) = write_volume_dir(&vol_dir, obj.string_data, bin) {
                            warn!(volume = %v.name, secret = %name, error = ?e, "failed to materialize Secret volume");
                            continue;
                        }
                        out.insert(v.name.clone(), ResolvedVolume::HostPath(vol_dir));
                    }
                    Err(_) if optional => {}
                    Err(e) => warn!(volume = %v.name, secret = %name, error = ?e, "failed to fetch Secret for volume"),
                }
            } else if let Some(empty_dir) = &v.empty_dir {
                if let Err(e) = std::fs::create_dir_all(&vol_dir) {
                    warn!(volume = %v.name, error = ?e, "failed to create emptyDir volume");
                    continue;
                }
                if is_memory_medium_empty_dir(empty_dir) {
                    let size_limit_bytes = empty_dir.size_limit.as_ref().and_then(parse_memory_bytes);
                    if let Err(e) = mount_tmpfs_empty_dir(&vol_dir, size_limit_bytes) {
                        warn!(volume = %v.name, error = ?e, "failed to mount tmpfs for a Memory-medium emptyDir volume; falling back to a plain disk directory");
                    }
                }
                out.insert(v.name.clone(), ResolvedVolume::HostPath(vol_dir));
            } else if let Some(downward) = &v.downward_api {
                if let Err(e) = write_downward_api_volume(&vol_dir, pod, downward.items.as_deref().unwrap_or(&[])) {
                    warn!(volume = %v.name, error = ?e, "failed to materialize downwardAPI volume");
                    continue;
                }
                out.insert(v.name.clone(), ResolvedVolume::HostPath(vol_dir));
            } else if let Some(projected) = &v.projected {
                if let Err(e) = self.write_projected_volume(&vol_dir, pod, id, projected).await {
                    warn!(volume = %v.name, error = ?e, "failed to materialize projected volume");
                    continue;
                }
                out.insert(v.name.clone(), ResolvedVolume::HostPath(vol_dir));
            } else if let Some(pvc_source) = &v.persistent_volume_claim {
                match self.resolve_csi_source(&id.namespace, &pvc_source.claim_name).await {
                    Ok(Some(mut source)) => {
                        source.read_only |= pvc_source.read_only.unwrap_or(false);
                        match self.csi.mount(&source, &vol_dir, &id.uid, false).await {
                            Ok(()) => {
                                out.insert(v.name.clone(), ResolvedVolume::HostPath(vol_dir));
                            }
                            Err(e) => warn!(volume = %v.name, claim = %pvc_source.claim_name, error = ?e, "failed to mount CSI volume"),
                        }
                    }
                    Ok(None) => {
                        // Not yet Bound, no CSI source, or no driver
                        // configured for it — resolve_csi_source() already
                        // warned with the specific reason. Same as any
                        // other unresolvable volume: silently absent from
                        // the mount map, container starts without it.
                    }
                    Err(e) => warn!(volume = %v.name, claim = %pvc_source.claim_name, error = ?e, "failed to resolve PersistentVolumeClaim"),
                }
            } else if v.ephemeral.is_some() {
                // Generic ephemeral volume (round 31): the actual PVC is
                // created by the ephemeral-volume controller (a
                // kube-controller-manager component), not nodelet — same
                // "not kubelet's job" boundary as dynamic provisioning
                // elsewhere in this file. Once that controller has created
                // it, it behaves exactly like a normal PVC reference, so
                // this reuses resolve_csi_source() for everything past the
                // ownership safety check.
                let claim_name = ephemeral_pvc_name(&id.name, &v.name);
                match self.resolve_ephemeral_source(&id.namespace, &claim_name, &id.uid).await {
                    Ok(Some(source)) => match self.csi.mount(&source, &vol_dir, &id.uid, false).await {
                        Ok(()) => {
                            out.insert(v.name.clone(), ResolvedVolume::HostPath(vol_dir));
                        }
                        Err(e) => warn!(volume = %v.name, claim = %claim_name, error = ?e, "failed to mount CSI volume for generic ephemeral volume"),
                    },
                    Ok(None) => {
                        // Not yet created by the ephemeral-volume
                        // controller, not owned by this pod, or otherwise
                        // unresolvable — resolve_ephemeral_source() already
                        // warned with the specific reason.
                    }
                    Err(e) => warn!(volume = %v.name, claim = %claim_name, error = ?e, "failed to resolve generic ephemeral volume's PersistentVolumeClaim"),
                }
            } else if let Some(csi_source) = &v.csi {
                // CSI ephemeral (inline) volume (round 46; found in round
                // 45's re-audit) — no PV/PVC at all, just this volume's own
                // CSIVolumeSource fields (e.g. secrets-store-csi-driver's
                // "mount a Secret from Vault directly" pattern).
                match self.resolve_csi_ephemeral_source(&id.namespace, csi_source, &id.uid, &v.name).await {
                    Some(source) => match self.csi.mount(&source, &vol_dir, &id.uid, true).await {
                        Ok(()) => {
                            out.insert(v.name.clone(), ResolvedVolume::HostPath(vol_dir));
                        }
                        Err(e) => warn!(volume = %v.name, driver = %csi_source.driver, error = ?e, "failed to mount CSI ephemeral volume"),
                    },
                    None => {
                        // No driver configured — resolve_csi_ephemeral_source()
                        // already warned with the specific reason.
                    }
                }
            } else if let Some(image_source) = &v.image {
                // volumeSource.image (round 32, KEP-4639): CRI has direct
                // native support for this via Mount.image — kubelet's own
                // job is just to PullImage the reference (respecting the
                // pod's imagePullSecrets, same as any container image) and
                // pass the runtime's resolved image_ref through; the
                // runtime does the actual mounting.
                match self.pull_image_volume(&id.namespace, pull_secrets, image_source).await {
                    Ok(resolved) => {
                        out.insert(v.name.clone(), resolved);
                    }
                    Err(e) => warn!(volume = %v.name, reference = %image_source.reference.as_deref().unwrap_or(""), error = ?e, "failed to pull image for image volume"),
                }
            } else {
                warn!(
                    volume = %v.name,
                    volume_type = volume_source_type(v),
                    pod = %format!("{}/{}", id.namespace, id.name),
                    "volume type not supported yet (configMap/secret/emptyDir/downwardAPI/projected are) — \
                     any container mounting it won't get this path");
            }
        }

        if let Some(aliases) = pod.spec.as_ref().and_then(|s| s.host_aliases.as_ref()).filter(|a| !a.is_empty()) {
            let hosts_path = pod_dir.join("etc-hosts");
            match write_etc_hosts(&hosts_path, aliases) {
                Ok(()) => {
                    out.insert(ETC_HOSTS_VOLUME_KEY.to_string(), ResolvedVolume::HostPath(hosts_path));
                }
                Err(e) => warn!(error = ?e, "failed to materialize /etc/hosts for hostAliases"),
            }
        }

        if let Some(fs_group) = pod
            .spec
            .as_ref()
            .and_then(|s| s.security_context.as_ref())
            .and_then(|sc| sc.fs_group)
        {
            for (key, source) in &out {
                if key == ETC_HOSTS_VOLUME_KEY {
                    continue; // a single file, not a directory nodelet materialized as a tree
                }
                // Image volumes (round 32) are read-only OCI content with
                // no host directory of nodelet's own to chown at all —
                // fsGroup doesn't apply to them, matching upstream.
                let ResolvedVolume::HostPath(dir) = source else { continue };
                if let Err(e) = apply_fs_group(dir, fs_group as u32) {
                    warn!(dir = %dir.display(), fs_group, error = ?e, "failed to apply fsGroup to volume");
                }
            }
        }

        out
    }

    /// Materialize a `projected` volume: each source contributes files into
    /// the same directory (real Kubernetes semantics — sources are merged,
    /// not nested). `serviceAccountToken`/`clusterTrustBundle` sources
    /// aren't implemented (the former needs the TokenRequest API; see
    /// docs/GAP_CLOSURE.md) — skipped with a warning, same treatment as any
    /// other unsupported volume type.
    async fn write_projected_volume(
        &self,
        dir: &std::path::Path,
        pod: &Pod,
        id: &PodId,
        projected: &k8s_openapi::api::core::v1::ProjectedVolumeSource,
    ) -> Result<()> {
        for source in projected.sources.as_deref().unwrap_or(&[]) {
            if let Some(cm) = &source.config_map {
                let optional = cm.optional.unwrap_or(false);
                match Api::<ConfigMap>::namespaced(self.client.clone(), &id.namespace).get(&cm.name).await {
                    Ok(obj) => {
                        let bin = obj.binary_data.map(|m| m.into_iter().map(|(k, v)| (k, v.0)).collect());
                        write_projected_keys(dir, obj.data, bin, cm.items.as_deref())?;
                    }
                    Err(_) if optional => {}
                    Err(e) => warn!(configmap = %cm.name, error = ?e, "projected volume: failed to fetch ConfigMap source"),
                }
            } else if let Some(sec) = &source.secret {
                let optional = sec.optional.unwrap_or(false);
                match Api::<Secret>::namespaced(self.client.clone(), &id.namespace).get(&sec.name).await {
                    Ok(obj) => {
                        let bin = obj.data.map(|m| m.into_iter().map(|(k, v)| (k, v.0)).collect());
                        write_projected_keys(dir, obj.string_data, bin, sec.items.as_deref())?;
                    }
                    Err(_) if optional => {}
                    Err(e) => warn!(secret = %sec.name, error = ?e, "projected volume: failed to fetch Secret source"),
                }
            } else if let Some(da) = &source.downward_api {
                write_downward_api_volume(dir, pod, da.items.as_deref().unwrap_or(&[]))?;
            } else if let Some(sat) = &source.service_account_token {
                let service_account = pod_service_account_name(pod);
                let audiences = token_audiences(sat.audience.as_deref());
                match self.resolve_service_account_token(&id.namespace, &service_account, &audiences, sat.expiration_seconds).await {
                    Ok(token) => {
                        let target = dir.join(&sat.path);
                        if let Some(parent) = target.parent() {
                            std::fs::create_dir_all(parent)?;
                        }
                        std::fs::write(target, token)?;
                    }
                    Err(e) => warn!(
                        pod = %format!("{}/{}", id.namespace, id.name),
                        service_account, error = ?e,
                        "projected volume: serviceAccountToken TokenRequest failed (RBAC needs `create` on serviceaccounts/token)"
                    ),
                }
            } else if source.cluster_trust_bundle.is_some() {
                warn!(
                    pod = %format!("{}/{}", id.namespace, id.name),
                    "projected volume: clusterTrustBundle source not supported"
                );
            }
        }
        Ok(())
    }

    /// Resolve a `spec.volumes[].ephemeral` (generic ephemeral) volume to
    /// its CSI source (round 31). Real kubelet doesn't create the backing
    /// PVC itself — that's the ephemeral-volume controller's job (a
    /// kube-controller-manager component), same "not kubelet's job"
    /// boundary as dynamic provisioning elsewhere in this file — so this
    /// only ever *reads* whatever that controller has already created at
    /// the deterministic name `ephemeral_pvc_name()` computes.
    ///
    /// Unlike `resolve_csi_source()` (used for a direct
    /// `persistentVolumeClaim` reference, where a missing PVC usually
    /// means a typo/misconfiguration worth surfacing as an error), a
    /// missing PVC here is the *expected*, normal state immediately after
    /// pod creation — the controller hasn't gotten to it yet — so this
    /// checks existence itself first and treats "doesn't exist yet" as a
    /// graceful `Ok(None)` retry, not a warning-level error.
    ///
    /// Also does the safety check `EphemeralVolumeSource`'s own API doc
    /// comment describes: a same-named PVC that isn't actually owned by
    /// this pod (checked by UID) is never used, even if bound and
    /// otherwise valid — avoids adopting an unrelated volume by mistake
    /// (e.g. a naming collision, or a leftover PVC from a previous pod).
    async fn resolve_ephemeral_source(
        &self,
        namespace: &str,
        claim_name: &str,
        pod_uid: &str,
    ) -> Result<Option<crate::runtime::csi::CsiVolumeSource>> {
        let pvc = match Api::<PersistentVolumeClaim>::namespaced(self.client.clone(), namespace).get_opt(claim_name).await {
            Ok(Some(pvc)) => pvc,
            Ok(None) => {
                warn!(claim = %claim_name, "generic ephemeral volume: PersistentVolumeClaim doesn't exist yet — waiting for the ephemeral-volume controller to create it; will retry next reconcile");
                return Ok(None);
            }
            Err(e) => return Err(e).with_context(|| format!("fetching PersistentVolumeClaim {claim_name}")),
        };
        if !pvc_owned_by_pod(&pvc, pod_uid) {
            warn!(claim = %claim_name, "generic ephemeral volume: a PersistentVolumeClaim with the expected name exists but isn't owned by this pod; refusing to use it (matches real kubelet's own safety check)");
            return Ok(None);
        }
        self.resolve_csi_source(namespace, claim_name).await
    }

    /// Resolve a `PersistentVolumeClaim` (by name, in `namespace`) to its
    /// bound `PersistentVolume`'s CSI source — `Ok(None)` (not an error) for
    /// every legitimate "nothing to mount yet" case: the PVC doesn't exist,
    /// isn't Bound yet (`.spec.volumeName` unset — this is normal right
    /// after pod creation if a provisioner is still creating the PV; the
    /// next reconcile tries again), the bound PV isn't backed by CSI at all
    /// (an in-tree volume type — out of scope for this slice), or no CSI
    /// driver is configured in `NODELET_CSI_DRIVERS` for it. Each case is
    /// logged with its specific reason so "why isn't my volume mounted"
    /// doesn't require reading source to answer.
    async fn resolve_csi_source(&self, namespace: &str, claim_name: &str) -> Result<Option<crate::runtime::csi::CsiVolumeSource>> {
        let pvc = match Api::<PersistentVolumeClaim>::namespaced(self.client.clone(), namespace).get(claim_name).await {
            Ok(pvc) => pvc,
            Err(e) => return Err(e).with_context(|| format!("fetching PersistentVolumeClaim {claim_name}")),
        };
        let Some(pv_name) = pvc.spec.as_ref().and_then(|s| s.volume_name.as_ref()) else {
            warn!(claim = %claim_name, "PersistentVolumeClaim not yet Bound to a PersistentVolume; will retry next reconcile");
            return Ok(None);
        };

        let pv = match Api::<PersistentVolume>::all(self.client.clone()).get(pv_name).await {
            Ok(pv) => pv,
            Err(e) => return Err(e).with_context(|| format!("fetching PersistentVolume {pv_name}")),
        };
        let Some(csi) = pv.spec.as_ref().and_then(|s| s.csi.as_ref()) else {
            warn!(claim = %claim_name, volume = %pv_name, "bound PersistentVolume has no .spec.csi source (an in-tree volume type isn't supported)");
            return Ok(None);
        };

        if !self.csi.driver_configured(&csi.driver) {
            warn!(claim = %claim_name, driver = %csi.driver, "no CSI driver configured for this PersistentVolume's driver — set NODELET_CSI_DRIVERS");
            return Ok(None);
        }

        let publish_context = if self.driver_requires_attach(&csi.driver).await {
            let attachments = Api::<VolumeAttachment>::all(self.client.clone())
                .list(&ListParams::default())
                .await
                .context("listing VolumeAttachments")?;
            match find_volume_attachment(&attachments.items, &csi.driver, &self.node_name, pv_name) {
                Some(att) => match attachment_publish_context(att) {
                    Some(ctx) => ctx,
                    None => {
                        warn!(claim = %claim_name, volume = %pv_name, driver = %csi.driver, "VolumeAttachment found but not yet attached; will retry next reconcile");
                        return Ok(None);
                    }
                },
                None => {
                    warn!(claim = %claim_name, volume = %pv_name, driver = %csi.driver, "driver requires attach but no matching VolumeAttachment exists yet (external-attacher hasn't created it); will retry next reconcile");
                    return Ok(None);
                }
            }
        } else {
            HashMap::new()
        };

        let node_stage_secrets = self.resolve_csi_secret_ref(csi.node_stage_secret_ref.as_ref(), namespace).await;
        let node_publish_secrets = self.resolve_csi_secret_ref(csi.node_publish_secret_ref.as_ref(), namespace).await;

        Ok(Some(crate::runtime::csi::CsiVolumeSource {
            driver: csi.driver.clone(),
            volume_handle: csi.volume_handle.clone(),
            fs_type: csi.fs_type.clone().unwrap_or_default(),
            read_only: csi.read_only.unwrap_or(false),
            volume_attributes: csi.volume_attributes.clone().unwrap_or_default().into_iter().collect(),
            node_stage_secrets,
            node_publish_secrets,
            publish_context,
        }))
    }

    /// Whether `driver` needs an attach before it can be staged/published —
    /// `CSIDriver.spec.attachRequired`, defaulting to "yes" (matching
    /// upstream: a driver with no `CSIDriver` object registered at all, or
    /// one that doesn't set the field, is assumed to require attach). Most
    /// node-local/edge storage drivers explicitly set `attachRequired:
    /// false` and skip this path entirely — this only matters for drivers
    /// backed by real block storage (cloud disks, SANs, ...).
    async fn driver_requires_attach(&self, driver: &str) -> bool {
        match Api::<CSIDriver>::all(self.client.clone()).get(driver).await {
            Ok(obj) => attach_required(Some(&obj)),
            Err(_) => attach_required(None),
        }
    }

    /// Resolve a CSI `SecretReference` (`nodeStageSecretRef`/
    /// `nodePublishSecretRef`) to key/value pairs for the CSI request's
    /// `secrets` map. Empty (not an error) if `reference` is `None` — most
    /// drivers don't need one at all. `SecretReference.namespace` is
    /// itself optional (PVs are cluster-scoped, so unlike every other
    /// Secret reference in this file there's no natural pod namespace to
    /// default to) — falls back to `default_namespace` (the PVC's own
    /// namespace) when unset, matching what most CSI driver docs assume.
    async fn resolve_csi_secret_ref(&self, reference: Option<&SecretReference>, default_namespace: &str) -> HashMap<String, String> {
        let Some(reference) = reference else { return HashMap::new() };
        let Some(name) = reference.name.as_deref() else { return HashMap::new() };
        let namespace = reference.namespace.as_deref().unwrap_or(default_namespace);
        match Api::<Secret>::namespaced(self.client.clone(), namespace).get(name).await {
            Ok(secret) => secret
                .data
                .unwrap_or_default()
                .into_iter()
                .map(|(k, v)| (k, String::from_utf8_lossy(&v.0).into_owned()))
                .collect(),
            Err(e) => {
                warn!(secret = %name, namespace, error = ?e, "CSI: failed to fetch a nodeStageSecretRef/nodePublishSecretRef Secret; proceeding without it");
                HashMap::new()
            }
        }
    }

    /// Resolve a CSI *ephemeral inline* volume (`volumes[].csi` specified
    /// directly — round 46; found in round 45's re-audit) — distinct from
    /// both the PVC path (`resolve_csi_source()`) and the generic
    /// `ephemeral` (PVC-templated) path (round 31): there's no PV/PVC
    /// object at all here, just the volume's own `CSIVolumeSource` fields.
    /// Real-world drivers like `secrets-store-csi-driver` use this form to
    /// mount secrets from an external store with no PVC involved.
    async fn resolve_csi_ephemeral_source(
        &self,
        namespace: &str,
        csi: &k8s_openapi::api::core::v1::CSIVolumeSource,
        pod_uid: &str,
        volume_name: &str,
    ) -> Option<crate::runtime::csi::CsiVolumeSource> {
        if !self.csi.driver_configured(&csi.driver) {
            warn!(driver = %csi.driver, volume = %volume_name, "CSI ephemeral volume: no CSI driver configured — set NODELET_CSI_DRIVERS or wait for it to register");
            return None;
        }
        let node_publish_secrets = match &csi.node_publish_secret_ref {
            Some(local_ref) => {
                let secret_ref = SecretReference { name: Some(local_ref.name.clone()), namespace: None };
                self.resolve_csi_secret_ref(Some(&secret_ref), namespace).await
            }
            None => HashMap::new(),
        };

        Some(crate::runtime::csi::CsiVolumeSource {
            driver: csi.driver.clone(),
            volume_handle: csi_ephemeral_volume_handle(pod_uid, volume_name),
            fs_type: csi.fs_type.clone().unwrap_or_default(),
            read_only: csi.read_only.unwrap_or(false),
            volume_attributes: csi.volume_attributes.clone().unwrap_or_default().into_iter().collect(),
            // Ephemeral inline volumes never stage (no NodeStageVolume) and
            // have no attach concept (no VolumeAttachment) — see
            // `CsiDrivers::mount()`'s `ephemeral` parameter.
            node_stage_secrets: HashMap::new(),
            node_publish_secrets,
            publish_context: HashMap::new(),
        })
    }

    /// Unpublish (and, if this was the last pod using it, unstage) every
    /// CSI-backed `PersistentVolumeClaim` volume this pod referenced.
    /// Best-effort per volume — one failing CSI driver call must not stop
    /// the rest of teardown, same treatment `graceful_stop_containers`
    /// already gives a failing `preStop` hook. Re-resolves the PVC->PV
    /// chain rather than remembering it from `ensure_pod()` time: simpler
    /// than a second side table, at the cost of a volume whose PVC/PV was
    /// deleted out from under a still-running pod not getting cleanly
    /// unmounted (logged, not silently lost — a real but narrow gap).
    async fn unmount_csi_volumes(&self, pod: &Pod, id: &PodId) {
        let Some(volumes) = pod.spec.as_ref().and_then(|s| s.volumes.as_ref()) else { return };
        let pod_dir = PathBuf::from(VOLUME_ROOT).join(&id.uid).join("volumes");

        for v in volumes {
            if let Some(csi_source) = &v.csi {
                // CSI ephemeral inline volume (round 46) — no PVC to
                // resolve; re-derive the same synthetic volume_handle
                // resolve_volumes() minted at mount time.
                let vol_dir = pod_dir.join(&v.name);
                let volume_handle = csi_ephemeral_volume_handle(&id.uid, &v.name);
                if let Err(e) = self.csi.unmount(&csi_source.driver, &volume_handle, &vol_dir, &id.uid, true).await {
                    warn!(volume = %v.name, driver = %csi_source.driver, error = ?e, "CSI teardown: failed to unmount ephemeral inline volume");
                }
                continue;
            }
            let claim_name = if let Some(pvc_source) = &v.persistent_volume_claim {
                pvc_source.claim_name.clone()
            } else if v.ephemeral.is_some() {
                // Generic ephemeral volume (round 31) — same deterministic
                // name ensure_pod()'s resolve_volumes() derives it by.
                ephemeral_pvc_name(&id.name, &v.name)
            } else {
                continue;
            };
            let source = match self.resolve_csi_source(&id.namespace, &claim_name).await {
                Ok(Some(source)) => source,
                Ok(None) => continue, // already logged why in resolve_csi_source()
                Err(e) => {
                    warn!(volume = %v.name, claim = %claim_name, error = ?e, "CSI teardown: failed to resolve PersistentVolumeClaim; volume left mounted");
                    continue;
                }
            };
            let vol_dir = pod_dir.join(&v.name);
            if let Err(e) = self.csi.unmount(&source.driver, &source.volume_handle, &vol_dir, &id.uid, false).await {
                warn!(volume = %v.name, driver = %source.driver, error = ?e, "CSI teardown: failed to unmount volume");
            }
        }
    }

    async fn resolve_service_env(&self, namespace: &str) -> Result<BTreeMap<String, Vec<u8>>> {
        let api: Api<Service> = Api::all(self.client.clone());
        let services = api.list(&ListParams::default()).await.context("listing Services")?;
        Ok(service_env_vars(&services.items, namespace))
    }

    async fn resolve_env_from(&self, source: &EnvFromSource, namespace: &str) -> Result<BTreeMap<String, Vec<u8>>> {
        let mut values = BTreeMap::new();
        let prefix = source.prefix.clone().unwrap_or_default();

        if let Some(reference) = &source.config_map_ref {
            let api: Api<ConfigMap> = Api::namespaced(self.client.clone(), namespace);
            let config_map = match api.get(&reference.name).await {
                Ok(obj) => obj,
                Err(_) if reference.optional.unwrap_or(false) => return Ok(values),
                Err(e) => return Err(e).with_context(|| format!("fetching ConfigMap {} for envFrom", reference.name)),
            };
            for (key, value) in config_map.data.unwrap_or_default() {
                values.insert(format!("{prefix}{key}"), value.into_bytes());
            }
        }

        if let Some(reference) = &source.secret_ref {
            let api: Api<Secret> = Api::namespaced(self.client.clone(), namespace);
            let secret = match api.get(&reference.name).await {
                Ok(obj) => obj,
                Err(_) if reference.optional.unwrap_or(false) => return Ok(values),
                Err(e) => return Err(e).with_context(|| format!("fetching Secret {} for envFrom", reference.name)),
            };
            for (key, value) in secret.data.unwrap_or_default() {
                values.insert(format!("{prefix}{key}"), value.0);
            }
        }

        Ok(values)
    }

    async fn resolve_env_var_source(
        &self,
        source: &EnvVarSource,
        pod: &Pod,
        id: &PodId,
        container: &k8s_openapi::api::core::v1::Container,
    ) -> Result<Option<Vec<u8>>> {
        if let Some(reference) = &source.config_map_key_ref {
            let api: Api<ConfigMap> = Api::namespaced(self.client.clone(), &id.namespace);
            let config_map = match api.get(&reference.name).await {
                Ok(obj) => obj,
                Err(_) if reference.optional.unwrap_or(false) => return Ok(None),
                Err(e) => return Err(e).with_context(|| format!("fetching ConfigMap {} for env", reference.name)),
            };
            return match config_map.data.unwrap_or_default().remove(&reference.key) {
                Some(value) => Ok(Some(value.into_bytes())),
                None if reference.optional.unwrap_or(false) => Ok(None),
                None => anyhow::bail!("ConfigMap {} has no key {}", reference.name, reference.key),
            };
        }

        if let Some(reference) = &source.secret_key_ref {
            let api: Api<Secret> = Api::namespaced(self.client.clone(), &id.namespace);
            let secret = match api.get(&reference.name).await {
                Ok(obj) => obj,
                Err(_) if reference.optional.unwrap_or(false) => return Ok(None),
                Err(e) => return Err(e).with_context(|| format!("fetching Secret {} for env", reference.name)),
            };
            return match secret.data.unwrap_or_default().remove(&reference.key) {
                Some(value) => Ok(Some(value.0)),
                None if reference.optional.unwrap_or(false) => Ok(None),
                None => anyhow::bail!("Secret {} has no key {}", reference.name, reference.key),
            };
        }

        if let Some(reference) = &source.field_ref {
            let value = pod_field_value(pod, &reference.field_path)
                .with_context(|| format!("unsupported or unavailable fieldRef {}", reference.field_path))?;
            return Ok(Some(value.into_bytes()));
        }

        if let Some(reference) = &source.resource_field_ref {
            let value = resolve_resource_field_ref(
                reference,
                container.resources.as_ref(),
                self.node_cpu_millicores,
                self.node_memory_bytes,
            )?;
            return Ok(Some(value.into_bytes()));
        }

        Ok(None)
    }

    async fn resolve_container_env(
        &self,
        pod: &Pod,
        id: &PodId,
        container: &k8s_openapi::api::core::v1::Container,
        service_env: &BTreeMap<String, Vec<u8>>,
    ) -> Result<Vec<KeyValue>> {
        let mut values = service_env.clone();

        for source in container.env_from.as_deref().unwrap_or(&[]) {
            for (key, value) in self.resolve_env_from(source, &id.namespace).await? {
                put_env(&mut values, key, value);
            }
        }

        for env in container.env.as_deref().unwrap_or(&[]) {
            if let Some(source) = &env.value_from {
                if let Some(value) = self.resolve_env_var_source(source, pod, id, container).await? {
                    values.insert(env.name.clone(), value);
                }
            } else {
                values.insert(env.name.clone(), env.value.clone().unwrap_or_default().into_bytes());
            }
        }

        Ok(values.into_iter().map(|(key, value)| KeyValue { key, value }).collect())
    }

    /// Look up our sandbox for a pod by namespace+name. These labels are always
    /// set (from real values), so this is the stable key — unlike `pod.uid`,
    /// which the agent does not have at status/teardown time.
    /// Returns the sandbox's id and CRI state (SANDBOX_READY / SANDBOX_NOTREADY
    /// as i32), not just existence — see ensure_pod()'s sandbox_reuse_decision()
    /// call for why the state matters: containerd's sandbox metadata can
    /// outlive its actual task/pause process (e.g. across a reboot — processes
    /// don't survive one, but the bolt-db record does), and reusing a
    /// not-ready sandbox as if it were live makes every CreateContainer
    /// against it fail forever with "no running task found".
    async fn find_sandbox(&self, namespace: &str, name: &str) -> Result<Option<(String, i32)>> {
        let mut rt = self.rt.clone();
        let filter = PodSandboxFilter {
            label_selector: HashMap::from([
                (POD_NS_LABEL.to_string(), namespace.to_string()),
                (POD_NAME_LABEL.to_string(), name.to_string()),
            ]),
            ..Default::default()
        };
        let resp = rt
            .list_pod_sandbox(ListPodSandboxRequest { filter: Some(filter) })
            .await?
            .into_inner();
        Ok(resp.items.into_iter().next().map(|s| (s.id, s.state)))
    }

    async fn list_pod_containers(&self, sandbox_id: &str) -> Result<Vec<v1::Container>> {
        let mut rt = self.rt.clone();
        let filter = ContainerFilter {
            pod_sandbox_id: sandbox_id.to_string(),
            ..Default::default()
        };
        let resp = rt
            .list_containers(ListContainersRequest { filter: Some(filter) })
            .await?
            .into_inner();
        Ok(resp.containers)
    }

    /// Drop every restart-count entry for a sandbox that's gone (removed or
    /// recreated-stale) — otherwise this side table grows forever across
    /// pod recreations.
    fn clear_restart_counts(&self, sandbox_id: &str) {
        clear_restart_counts_in(&mut self.restart_counts.lock().unwrap(), sandbox_id);
    }

    fn restart_count(&self, sandbox_id: &str, container_name: &str) -> u32 {
        restart_count_from(&self.restart_counts.lock().unwrap(), sandbox_id, container_name)
    }

    /// Bump and return the new restart count for a container that's about
    /// to be recreated after actually having existed before (not the very
    /// first creation — see the `NeedsRestart` branches' `existing_ctr` check).
    fn bump_restart_count(&self, sandbox_id: &str, container_name: &str) -> u32 {
        bump_restart_count_in(&mut self.restart_counts.lock().unwrap(), sandbox_id, container_name)
    }

    /// Find a container's CRI id within a sandbox by its `nodelet.dev/container-name` label.
    async fn find_container_id(&self, sandbox_id: &str, container_name: &str) -> Result<Option<String>> {
        let existing = self.list_pod_containers(sandbox_id).await?;
        Ok(existing
            .into_iter()
            .find(|c| c.labels.get(CTR_NAME_LABEL).map(|n| n == container_name).unwrap_or(false))
            .map(|c| c.id))
    }

    async fn run_sandbox(
        &self,
        id: &PodId,
        hostname: &str,
        sysctls: &HashMap<String, String>,
        dns: Option<DnsConfig>,
        runtime_handler: String,
        cgroup_parent: String,
        overhead: Option<LinuxContainerResources>,
    ) -> Result<String> {
        let mut rt = self.rt.clone();
        // spec.hostUsers: false (round 25) — allocate this pod an exclusive
        // host UID/GID range (keyed by pod uid, stable across reconciles/
        // retries; sandbox_id doesn't exist yet at this point). Allocation
        // failure (pool exhausted) falls back to no user namespace with a
        // warning rather than failing pod creation outright — the same
        // graceful-degradation posture CPU/Memory Manager already have.
        let userns_mapping = if !id.host_users {
            match self.userns.allocate(&id.uid) {
                Some(mapping) => Some(mapping),
                None => {
                    warn!(pod = %format!("{}/{}", id.namespace, id.name), "user namespace: no free UID/GID range available; falling back to the host user namespace");
                    None
                }
            }
        } else {
            None
        };
        let mut config = sandbox_config(id, userns_mapping, hostname, sysctls);
        config.dns_config = dns;
        let linux = config.linux.get_or_insert_with(LinuxPodSandboxConfig::default);
        linux.cgroup_parent = cgroup_parent;
        linux.overhead = overhead;
        let resp = rt
            .run_pod_sandbox(RunPodSandboxRequest { config: Some(config), runtime_handler })
            .await?
            .into_inner();
        Ok(resp.pod_sandbox_id)
    }

    /// Resolve `spec.runtimeClassName` to a CRI runtime handler name (e.g.
    /// `gvisor`, `kata`) via the cluster-scoped `RuntimeClass` object. Empty
    /// string (CRI's "use the default handler") if unset, the referenced
    /// RuntimeClass doesn't exist, or the lookup fails — a missing
    /// RuntimeClass should really block scheduling (real k8s validates this
    /// at admission), but nodelet doesn't implement that admission check,
    /// so falling back to the default runtime is safer than refusing to run
    /// the pod at all over a lookup it can't itself enforce.
    async fn resolve_runtime_handler(&self, pod: &Pod) -> String {
        let Some(class_name) = pod.spec.as_ref().and_then(|s| s.runtime_class_name.as_deref()) else {
            return String::new();
        };
        let api: Api<RuntimeClass> = Api::all(self.client.clone());
        match api.get(class_name).await {
            Ok(rc) => rc.handler,
            Err(e) => {
                warn!(runtime_class = %class_name, error = ?e, "failed to resolve RuntimeClass; using the default runtime handler");
                String::new()
            }
        }
    }

    // Restart-on-exit: without this, a container that crashes (any reason —
    // app bug, a bad Corefile, transient resource pressure) sits exited
    // forever, `already` matches by name alone regardless of state, and
    // ensure_container becomes a permanent no-op for it. build_status() then
    // sees "all containers exited" and reports the *Pod* as Succeeded — a
    // terminal phase Kubernetes' ReplicaSet controller treats as permanently
    // inactive (isPodActive excludes Succeeded/Failed), so it creates a
    // replacement. Forever, once per crash. Confirmed for real: this is
    // exactly what was driving unbounded coredns pod creation — coredns's
    // container was exiting seconds after starting, nodelet never restarted
    // it, and every single exit silently manufactured a brand new pod
    // instead of the crash-looping restart-in-place a real kubelet gives a
    // restartPolicy: Always pod (the default, and what every Deployment
    // uses). "Never" is left alone — matches the one-shot Job-style pods
    // that policy is for.
    async fn ensure_container(
        &self,
        sandbox_id: &str,
        id: &PodId,
        container: &Container,
        pod_sc: Option<&PodSecurityContext>,
        restart_policy: &str,
        volumes: &HashMap<String, ResolvedVolume>,
        pull_secrets: &[String],
        envs: &[KeyValue],
        qos: QosClass,
    ) -> Result<()> {
        // Resize status reporting (round 43): record what the pod spec is
        // currently asking for, every reconcile, regardless of whether a
        // resize below succeeds/fails/isn't needed — nodelet has no
        // admission/deferral layer, so "allocated" always just mirrors the
        // live spec. Reported as `containerStatuses[].allocatedResources`.
        self.spec_resources.lock().unwrap().insert(
            restart_count_key(sandbox_id, &container.name),
            container.resources.as_ref().and_then(|r| r.requests.clone()).unwrap_or_default(),
        );

        let running_v = ContainerState::ContainerRunning as i32;
        let existing = self.list_pod_containers(sandbox_id).await?;
        let existing_ctr = existing
            .iter()
            .find(|c| c.labels.get(CTR_NAME_LABEL).map(|n| n == &container.name).unwrap_or(false));

        let needs_restart;
        match restart_decision(existing_ctr.map(|c| c.state), running_v, restart_policy) {
            RestartDecision::LeaveTerminated => return Ok(()),
            RestartDecision::AlreadyRunning => {
                // In-place pod vertical scaling (round 42; found in round 39's
                // re-audit): the container's live resources may no longer
                // match its (possibly just-edited) pod spec. Compare against
                // the resources actually last applied (`container_resources`,
                // already tracked for CPU Manager's shared-pool refresh —
                // round 16) rather than recomputing from scratch, so a
                // CPU/Memory Manager-driven cpuset change is never itself
                // mistaken for a resize request (`resize_decision()` only
                // looks at the pod-spec-derived fields).
                let key = restart_count_key(sandbox_id, &container.name);
                let recorded = self.container_resources.lock().unwrap().get(&key).cloned();
                if let Some((container_id, actual)) = recorded {
                    let desired = linux_resources(container.resources.as_ref(), qos, self.node_memory_bytes);
                    match resize_decision(&desired, &actual, container.resize_policy.as_deref()) {
                        ResizeDecision::NoChange => return Ok(()),
                        ResizeDecision::UpdateInPlace => {
                            let mut updated = actual;
                            updated.cpu_shares = desired.cpu_shares;
                            updated.cpu_quota = desired.cpu_quota;
                            updated.cpu_period = desired.cpu_period;
                            updated.memory_limit_in_bytes = desired.memory_limit_in_bytes;
                            updated.oom_score_adj = desired.oom_score_adj;
                            let mut rt = self.rt.clone();
                            match rt
                                .update_container_resources(UpdateContainerResourcesRequest {
                                    container_id: container_id.clone(),
                                    linux: Some(updated.clone()),
                                    ..Default::default()
                                })
                                .await
                            {
                                Ok(_) => {
                                    self.container_resources.lock().unwrap().insert(key.clone(), (container_id, updated));
                                    self.applied_resources.lock().unwrap().insert(key, container.resources.clone().unwrap_or_default());
                                }
                                Err(e) => {
                                    warn!(container = %container.name, error = ?e, "in-place resize: UpdateContainerResources failed; leaving the container's resources unchanged for now");
                                }
                            }
                            return Ok(());
                        }
                        ResizeDecision::RequiresRestart => needs_restart = true,
                    }
                } else {
                    return Ok(()); // nothing recorded yet (shouldn't happen for a running container) — nothing to compare against
                }
            }
            RestartDecision::NeedsRestart => needs_restart = true,
        }

        if needs_restart {
            // Not running (or a resize policy demanded a restart) and this
            // pod is allowed to restart — clear the stale container out (if
            // there was one) so the create-below gets a fresh one.
            // Best-effort: if it's already gone by the time we ask, or CRI
            // won't remove it for some other reason, fall through and let
            // CreateContainer surface any real problem instead of masking
            // it here.
            if let Some(c) = existing_ctr {
                self.bump_restart_count(sandbox_id, &container.name);
                self.release_container_devices(sandbox_id, &container.name).await;
                let mut rt = self.rt.clone();
                let _ = rt.remove_container(RemoveContainerRequest { container_id: c.id.clone() }).await;
            }
        }

        let attempt = self.restart_count(sandbox_id, &container.name);
        self.create_and_start_container(
            sandbox_id, id, container, pod_sc, volumes, pull_secrets, envs, ContainerKind::App, attempt, qos,
        )
        .await
    }

    /// Ephemeral (debug) containers are one-shot: unlike app containers,
    /// once one exists (running or exited) it's never recreated or
    /// restarted, no matter the pod's `restartPolicy` — matches real
    /// kubelet, which doesn't support removing or re-running a debug
    /// container once added.
    async fn ensure_ephemeral_container(
        &self,
        sandbox_id: &str,
        id: &PodId,
        pod: &Pod,
        container: &Container,
        pod_sc: Option<&PodSecurityContext>,
        volumes: &HashMap<String, ResolvedVolume>,
        pull_secrets: &[String],
        service_env: &BTreeMap<String, Vec<u8>>,
    ) -> Result<()> {
        let existing = self.list_pod_containers(sandbox_id).await?;
        let already_exists = existing.iter().any(|c| {
            c.labels.get(CTR_NAME_LABEL).map(|n| n == &container.name).unwrap_or(false)
                && c.labels.get(CTR_EPHEMERAL_LABEL).map(|v| v == "true").unwrap_or(false)
        });
        if already_exists {
            return Ok(());
        }
        let envs = self.resolve_container_env(pod, id, container, service_env).await?;
        let qos = crate::eviction::qos_class(pod);
        self.create_and_start_container(
            sandbox_id, id, container, pod_sc, volumes, pull_secrets, &envs, ContainerKind::Ephemeral, 0, qos,
        )
        .await
    }

    /// Resolve `{username, password}` for pulling `image` out of the given
    /// `imagePullSecrets` (by name, in the Pod's namespace) — the first
    /// secret with a matching registry host wins, same order kubelet itself
    /// tries them in. `None` if none match (or none are configured), in
    /// which case `PullImageRequest.auth` is left unset — fine for public
    /// images, the pre-existing behavior for everything until now.
    /// Mint a real, apiserver-signed ServiceAccount token via the
    /// `serviceaccounts/token` subresource (the `TokenRequest` API) — this
    /// is what backs a projected `serviceAccountToken` volume, the same
    /// mechanism every current `kube-api-access-*` volume uses on real
    /// Kubernetes. k8s-openapi 0.28 doesn't generate a typed helper for this
    /// subresource, so it's a raw POST via `kube::Client::request`.
    /// Needs nodelet's own client identity to have `create` on
    /// `serviceaccounts/token` in the target namespace — a real RBAC
    /// requirement, not a nodelet limitation; callers log and skip on
    /// failure rather than treating it as fatal to the whole pod.
    async fn resolve_service_account_token(
        &self,
        namespace: &str,
        service_account: &str,
        audiences: &[String],
        expiration_seconds: Option<i64>,
    ) -> Result<String> {
        use k8s_openapi::api::authentication::v1::{TokenRequest, TokenRequestSpec};
        let body = TokenRequest {
            metadata: Default::default(),
            spec: TokenRequestSpec {
                audiences: audiences.to_vec(),
                bound_object_ref: None,
                expiration_seconds,
            },
            status: None,
        };
        let bytes = serde_json::to_vec(&body).context("serializing TokenRequest")?;
        let req = http::Request::builder()
            .method("POST")
            .uri(format!("/api/v1/namespaces/{namespace}/serviceaccounts/{service_account}/token"))
            .header("Content-Type", "application/json")
            .body(bytes)
            .context("building TokenRequest HTTP request")?;
        let resp: TokenRequest = self.client.request(req).await.context("TokenRequest API call")?;
        resp.status.map(|s| s.token).context("TokenRequest response had no status.token")
    }

    async fn resolve_pull_auth(&self, namespace: &str, pull_secrets: &[String], image: &str) -> Option<AuthConfig> {
        let registry_host = registry_host_for_image(image);
        for name in pull_secrets {
            let Ok(secret) = Api::<Secret>::namespaced(self.client.clone(), namespace).get(name).await else {
                continue;
            };
            let Some(bytes) = secret.data.as_ref().and_then(|d| d.get(".dockerconfigjson")).map(|b| b.0.clone())
            else {
                continue;
            };
            if let Some((username, password)) = parse_dockerconfigjson(&bytes, &registry_host) {
                return Some(AuthConfig { username, password, ..Default::default() });
            }
        }
        None
    }

    /// Pull `source.reference` (respecting the pod's `imagePullSecrets`,
    /// the same as any container image — `resolve_pull_auth()`) for a
    /// `volumeSource.image` volume (round 32, KEP-4639) and return the
    /// `ResolvedVolume::Image` CRI's own `Mount.image` field needs.
    /// `image_ref` comes from the runtime's own `PullImageResponse`, not
    /// the raw `source.reference` — matching the CRI proto's documented
    /// contract ("evaluates the returned `PullImageResponse.image_ref`
    /// value, which is then set to the `ImageSpec.image` field").
    async fn pull_image_volume(
        &self,
        namespace: &str,
        pull_secrets: &[String],
        source: &k8s_openapi::api::core::v1::ImageVolumeSource,
    ) -> Result<ResolvedVolume> {
        let reference = source.reference.clone().filter(|r| !r.is_empty()).context("image volume has no .reference set")?;
        let auth = self.resolve_pull_auth(namespace, pull_secrets, &reference).await;
        let mut img = self.img.clone();
        let resp = img
            .pull_image(PullImageRequest {
                image: Some(ImageSpec { image: reference, ..Default::default() }),
                auth,
                sandbox_config: None,
            })
            .await
            .context("PullImage for image volume")?
            .into_inner();
        Ok(ResolvedVolume::Image { image_ref: resp.image_ref })
    }

    /// Record which devices ended up backing a container, keyed the same
    /// way `restart_counts` is — so a later restart/removal can find and
    /// release them without re-deriving anything.
    fn record_device_allocations(&self, sandbox_id: &str, container_name: &str, allocations: Vec<(String, Vec<String>)>) {
        if allocations.is_empty() {
            return;
        }
        self.device_allocations.lock().unwrap().insert(restart_count_key(sandbox_id, container_name), allocations);
    }

    /// Give back every device allocation this list represents — used both
    /// when a just-attempted allocation needs to be unwound (container
    /// creation/start failed after devices were already picked) and as the
    /// shared tail end of `release_container_devices()`/
    /// `release_sandbox_devices()` below.
    fn release_devices(&self, allocations: &[(String, Vec<String>)]) {
        for (resource_name, device_ids) in allocations {
            self.device_plugins.release(resource_name, device_ids);
        }
    }

    /// Release and forget every device-plugin allocation *and* CPU Manager
    /// exclusive claim recorded for one container — call before recreating
    /// a container (restart-on-exit) or removing it outright, so both go
    /// back to their respective pools instead of being stranded as
    /// permanently "in use." Also drops the container from
    /// `container_resources` (it's gone, nothing left to refresh) and, if
    /// it held an exclusive CPU claim, sweeps the shared pool so its cores
    /// are actually usable by whatever's already running rather than just
    /// theoretically free.
    async fn release_container_devices(&self, sandbox_id: &str, container_name: &str) {
        let key = restart_count_key(sandbox_id, container_name);
        if let Some(allocations) = self.device_allocations.lock().unwrap().remove(&key) {
            self.release_devices(&allocations);
        }
        self.container_resources.lock().unwrap().remove(&key);
        self.applied_resources.lock().unwrap().remove(&key);
        self.spec_resources.lock().unwrap().remove(&key);
        if let Some(cpu_manager) = &self.cpu_manager {
            let was_exclusive = cpu_manager.is_exclusive(&key);
            cpu_manager.release(&key);
            if was_exclusive {
                self.refresh_shared_pool_cpusets().await;
            }
        }
        if let Some(memory_manager) = &self.memory_manager {
            memory_manager.release(&key);
        }
    }

    /// Same, for every container in a sandbox that's being torn down —
    /// mirrors `clear_restart_counts()`'s prefix-based sweep.
    async fn release_sandbox_devices(&self, sandbox_id: &str) {
        let prefix = format!("{sandbox_id}/");
        let removed: Vec<Vec<(String, Vec<String>)>> = {
            let mut table = self.device_allocations.lock().unwrap();
            let keys: Vec<String> = table.keys().filter(|k| k.starts_with(&prefix)).cloned().collect();
            keys.into_iter().filter_map(|k| table.remove(&k)).collect()
        };
        for allocations in removed {
            self.release_devices(&allocations);
        }
        self.container_resources.lock().unwrap().retain(|k, _| !k.starts_with(&prefix));
        self.applied_resources.lock().unwrap().retain(|k, _| !k.starts_with(&prefix));
        self.spec_resources.lock().unwrap().retain(|k, _| !k.starts_with(&prefix));
        if let Some(cpu_manager) = &self.cpu_manager {
            // Unconditionally refresh (unlike release_container_devices,
            // which only bothers when it knows a single container was
            // exclusive) — a sandbox can hold several containers, cheaper
            // to just always sweep once than track whether any of them
            // held a claim before release_sandbox() below forgets that.
            cpu_manager.release_sandbox(sandbox_id);
            self.refresh_shared_pool_cpusets().await;
        }
        if let Some(memory_manager) = &self.memory_manager {
            memory_manager.release_sandbox(sandbox_id);
        }
    }

    /// CPU Manager's retroactive half: bring every currently-tracked,
    /// non-exclusively-pinned container's `cpuset_cpus` in line with the
    /// current shared pool, via CRI's `UpdateContainerResources`. Called
    /// after any exclusive claim or release changes what the shared pool
    /// actually is. No-op if the policy is disabled. Best-effort per
    /// container — one runtime error updating a stale/gone container must
    /// not stop the rest from being refreshed; `container_resources` is
    /// only updated for entries that were actually applied successfully,
    /// so a failed update gets retried on the next pool change instead of
    /// nodelet believing it already happened.
    async fn refresh_shared_pool_cpusets(&self) {
        let Some(cpu_manager) = &self.cpu_manager else { return };
        let shared = crate::cpu_manager::format_cpuset(&cpu_manager.shared_pool());

        let entries: Vec<(String, String, LinuxContainerResources)> = self
            .container_resources
            .lock()
            .unwrap()
            .iter()
            .map(|(key, (container_id, resources))| (key.clone(), container_id.clone(), resources.clone()))
            .collect();

        let mut rt = self.rt.clone();
        for (key, container_id, mut resources) in entries {
            if cpu_manager.is_exclusive(&key) || resources.cpuset_cpus == shared {
                continue; // exclusively-pinned containers keep their own dedicated set; already-correct ones need no call
            }
            resources.cpuset_cpus = shared.clone();
            match rt
                .update_container_resources(UpdateContainerResourcesRequest {
                    container_id: container_id.clone(),
                    linux: Some(resources.clone()),
                    ..Default::default()
                })
                .await
            {
                Ok(_) => {
                    self.container_resources.lock().unwrap().insert(key, (container_id, resources));
                }
                Err(e) => {
                    warn!(container_id, error = ?e, "CPU Manager: failed to refresh a shared-pool container's cpuset; will retry on the next pool change");
                }
            }
        }
    }

    /// The actual pull+create+start, shared by app containers
    /// (`ensure_container`) and init containers (`ensure_init_containers`) —
    /// they differ only in *when* to call this and what to do with an
    /// already-existing container, not in how a fresh one gets built.
    async fn create_and_start_container(
        &self,
        sandbox_id: &str,
        id: &PodId,
        container: &Container,
        pod_sc: Option<&PodSecurityContext>,
        volumes: &HashMap<String, ResolvedVolume>,
        pull_secrets: &[String],
        envs: &[KeyValue],
        kind: ContainerKind,
        attempt: u32,
        qos: QosClass,
    ) -> Result<()> {
        let image = container.image.clone().unwrap_or_default();
        let auth = self.resolve_pull_auth(&id.namespace, pull_secrets, &image).await;
        let image_spec = ImageSpec { image: image.clone(), ..Default::default() };

        // imagePullPolicy (round 51; found in round 50's re-audit): `Always`
        // still pulls unconditionally (containerd itself no-ops if the
        // digest is already current) — the point of `IfNotPresent`/`Never`
        // is avoiding the *network round-trip* to the registry entirely,
        // which matters on a genuinely offline edge device even when the
        // image is already cached locally.
        let policy = effective_pull_policy(container.image_pull_policy.as_deref(), &image);
        let mut img = self.img.clone();
        let already_present = if policy != "Always" {
            img.image_status(ImageStatusRequest { image: Some(image_spec.clone()), verbose: false })
                .await
                .ok()
                .and_then(|r| r.into_inner().image)
                .is_some()
        } else {
            false
        };
        let need_pull = match policy {
            "Always" => true,
            "Never" => {
                if !already_present {
                    anyhow::bail!("imagePullPolicy: Never, but image '{image}' is not present on this node");
                }
                false
            }
            _ => !already_present, // IfNotPresent
        };
        if need_pull {
            img.pull_image(PullImageRequest {
                image: Some(image_spec.clone()),
                auth,
                sandbox_config: Some(sandbox_config(id, None, &id.name, &HashMap::new())),
            })
            .await
            .context("pulling image")?;
        }

        let mut mounts = build_mounts(container.volume_mounts.as_deref().unwrap_or(&[]), volumes);
        if let Some(ResolvedVolume::HostPath(hosts_path)) = volumes.get(ETC_HOSTS_VOLUME_KEY) {
            mounts.push(Mount {
                container_path: "/etc/hosts".to_string(),
                host_path: hosts_path.to_string_lossy().into_owned(),
                readonly: false,
                ..Default::default()
            });
        }
        // `terminationMessagePath` (round 24): bind-mount an empty host file
        // in at the container's requested path (default `/dev/termination-log`,
        // matching the apiserver's own defaulting) so nodelet can read
        // whatever the container writes there back out after it exits — the
        // same host-file-bind-mount approach real kubelet uses, not a CRI
        // concept at all. App and init containers only; ephemeral/debug
        // containers keep round 8's existing "exit codes not tracked"
        // simplification (see `build_labeled_container_statuses()`).
        if matches!(kind, ContainerKind::App | ContainerKind::Init) {
            let host_path = termination_message_host_path(&id.uid, &container.name);
            if let Some(parent) = host_path.parent() {
                std::fs::create_dir_all(parent).context("creating termination-message host directory")?;
            }
            if !host_path.exists() {
                std::fs::File::create(&host_path).context("creating termination-message host file")?;
            }
            let container_path =
                container.termination_message_path.clone().filter(|p| !p.is_empty()).unwrap_or_else(|| "/dev/termination-log".to_string());
            mounts.push(Mount { container_path, host_path: host_path.to_string_lossy().into_owned(), readonly: false, ..Default::default() });
        }
        let mut resources = linux_resources(container.resources.as_ref(), qos, self.node_memory_bytes);
        let limits = container.resources.as_ref().and_then(|r| r.limits.as_ref());
        let cpu_limit = limits.and_then(|m| m.get("cpu")).and_then(parse_cpu_millicores);
        let mem_limit = limits.and_then(|m| m.get("memory")).and_then(parse_memory_bytes);
        let wants_exclusive_cpus = crate::cpu_manager::wants_exclusive_cpus(qos, cpu_limit);
        let wants_pinned_memory = crate::memory_manager::wants_pinned_memory(qos, mem_limit);
        let device_requests: Vec<(String, u64)> =
            extended_resource_requests(limits).into_iter().filter(|(name, _)| self.device_plugins.resource_configured(name)).collect();

        // Topology Manager (opt-in — see topology.rs): find a single NUMA
        // node that can satisfy this container's exclusive-CPU want (if
        // any), pinned-memory want (if any), and every device-plugin
        // resource it needs (if any), so they don't end up scattered
        // across nodes. A no-op (nothing preferred, exactly pre-round-17
        // behavior) when the policy is `none`, or when this container has
        // nothing for it to coordinate at all. `Restricted` (round 20)
        // falls back to `topology::spread()` — each provider placed on its
        // own best node independently — when no single node works for
        // everyone; `SingleNumaNode` never does (see topology.rs).
        enum HintKind {
            Cpu,
            Memory,
            Device(String),
        }
        let mut cpu_preferred_node: Option<u32> = None;
        let mut memory_preferred_node: Option<u32> = None;
        let mut device_preferred_nodes: HashMap<String, u32> = HashMap::new();
        if self.topology_policy != crate::topology::TopologyManagerPolicy::None {
            let mut hints = Vec::new();
            let mut hint_kinds = Vec::new();
            if let (Some(count), Some(cpu_manager)) = (wants_exclusive_cpus, &self.cpu_manager) {
                hints.push(crate::topology::cpu_hint(&self.numa_topology, &cpu_manager.shared_pool(), count));
                hint_kinds.push(HintKind::Cpu);
            }
            if let (Some(bytes), Some(memory_manager)) = (wants_pinned_memory, &self.memory_manager) {
                hints.push(crate::topology::memory_hint(&memory_manager.free_per_node(), bytes));
                hint_kinds.push(HintKind::Memory);
            }
            for (resource_name, count) in &device_requests {
                let available = self.device_plugins.available_device_numa_nodes(resource_name);
                let all_nodes: std::collections::BTreeSet<u32> = self.numa_topology.keys().copied().collect();
                hints.push(crate::topology::device_hint(&available, &all_nodes, *count as u32));
                hint_kinds.push(HintKind::Device(resource_name.clone()));
            }
            if !hints.is_empty() {
                let apply = |node: u32, kind: &HintKind, cpu: &mut Option<u32>, mem: &mut Option<u32>, dev: &mut HashMap<String, u32>| match kind {
                    HintKind::Cpu => *cpu = Some(node),
                    HintKind::Memory => *mem = Some(node),
                    HintKind::Device(name) => {
                        dev.insert(name.clone(), node);
                    }
                };
                match crate::topology::align(&hints) {
                    Some(node) => {
                        for kind in &hint_kinds {
                            apply(node, kind, &mut cpu_preferred_node, &mut memory_preferred_node, &mut device_preferred_nodes);
                        }
                    }
                    None => match self.topology_policy {
                        crate::topology::TopologyManagerPolicy::SingleNumaNode => {
                            anyhow::bail!(
                                "Topology Manager: no single NUMA node can satisfy container '{}'s CPU/memory/device requests together",
                                container.name
                            );
                        }
                        crate::topology::TopologyManagerPolicy::Restricted => match crate::topology::spread(&hints) {
                            Some(nodes) => {
                                for (kind, node) in hint_kinds.iter().zip(nodes) {
                                    apply(node, kind, &mut cpu_preferred_node, &mut memory_preferred_node, &mut device_preferred_nodes);
                                }
                                warn!(container = %container.name, "Topology Manager: no single NUMA node satisfies every request together; spreading each across its own best node (restricted policy)");
                            }
                            None => {
                                anyhow::bail!(
                                    "Topology Manager: some request in container '{}' can't be satisfied on any NUMA node at all",
                                    container.name
                                );
                            }
                        },
                        crate::topology::TopologyManagerPolicy::BestEffort => {
                            warn!(container = %container.name, "Topology Manager: no aligned NUMA node found; proceeding without alignment (best-effort policy)");
                        }
                        crate::topology::TopologyManagerPolicy::None => unreachable!("guarded above"),
                    },
                }
            }
        }
        let preferred_cpus = cpu_preferred_node.and_then(|node| self.numa_topology.get(&node));

        // CPU Manager (static policy, opt-in — see cpu_manager.rs): a
        // Guaranteed-QoS container requesting a whole number of CPUs gets
        // pinned to exclusive cores (preferring the Topology Manager's
        // aligned NUMA node, if any); every other container gets the
        // current shared pool (everything except reserved + exclusively-
        // claimed cores) instead of being left unconstrained. Both are
        // no-ops when the policy is disabled (self.cpu_manager is None).
        let mut cpu_manager_key: Option<String> = None;
        if let Some(cpu_manager) = &self.cpu_manager {
            let key = restart_count_key(sandbox_id, &container.name);
            let cpuset = match wants_exclusive_cpus {
                Some(count) => match cpu_manager.allocate_preferring(&key, count, preferred_cpus) {
                    Some(cpus) => {
                        cpu_manager_key = Some(key);
                        cpus
                    }
                    None => {
                        warn!(container = %container.name, wanted = count, "CPU Manager: not enough exclusive CPUs available; falling back to the shared pool");
                        cpu_manager.shared_pool()
                    }
                },
                None => cpu_manager.shared_pool(),
            };
            resources.cpuset_cpus = crate::cpu_manager::format_cpuset(&cpuset);
        }

        // Memory Manager (static policy, opt-in — see memory_manager.rs):
        // a Guaranteed-QoS container with a memory limit set gets its
        // memory pinned to a single NUMA node (preferring the Topology
        // Manager's aligned node, if any). Unlike CPU Manager, non-pinned
        // containers are left with `cpuset_mems` unset ("unconstrained")
        // rather than tracked in a shared pool — see memory_manager.rs's
        // module doc comment for why. A no-op when the policy is disabled
        // (self.memory_manager is None).
        let mut memory_manager_key: Option<String> = None;
        if let (Some(bytes), Some(memory_manager)) = (wants_pinned_memory, &self.memory_manager) {
            let key = restart_count_key(sandbox_id, &container.name);
            match memory_manager.allocate_preferring(&key, bytes, memory_preferred_node) {
                Some(node) => {
                    memory_manager_key = Some(key);
                    resources.cpuset_mems = node.to_string();
                }
                None => {
                    warn!(container = %container.name, wanted = bytes, "Memory Manager: no single NUMA node has enough free capacity; leaving memory unpinned");
                }
            }
        }

        let resources_for_record = resources.clone();
        let linux = Some(LinuxContainerConfig {
            resources: Some(resources),
            security_context: Some(linux_security_context(
                pod_sc,
                container.security_context.as_ref(),
                pid_namespace_mode(id.host_pid, id.share_process_namespace),
            )),
        });

        // Device plugin resources (nvidia.com/gpu and similar): allocate
        // specific devices (preferring the Topology Manager's aligned NUMA
        // node, if any) for each resource this container's limits name
        // that a registered device plugin actually backs, and merge in
        // whatever envs/mounts/device-nodes/annotations the plugin's
        // Allocate() RPC says to inject. Best-effort per resource — a
        // plugin failure means the container starts without that device
        // rather than failing the whole pod, logged clearly either way.
        let mut envs = envs.to_vec();
        let mut devices = Vec::new();
        let mut annotations = HashMap::new();
        let mut allocated_devices: Vec<(String, Vec<String>)> = Vec::new();
        for (resource_name, count) in device_requests {
            let preferred = device_preferred_nodes.get(&resource_name).copied();
            match self.device_plugins.allocate_preferring(&resource_name, count, preferred).await {
                Ok((device_ids, resp)) => {
                    envs.extend(resp.envs.into_iter().map(|(key, value)| KeyValue { key, value: value.into_bytes() }));
                    mounts.extend(resp.mounts.into_iter().map(|m| Mount {
                        container_path: m.container_path,
                        host_path: m.host_path,
                        readonly: m.read_only,
                        ..Default::default()
                    }));
                    devices.extend(resp.devices.into_iter().map(|d| v1::Device {
                        container_path: d.container_path,
                        host_path: d.host_path,
                        permissions: d.permissions,
                    }));
                    annotations.extend(resp.annotations);
                    allocated_devices.push((resource_name, device_ids));
                }
                Err(e) => {
                    warn!(container = %container.name, resource = %resource_name, error = ?e, "device plugin Allocate() failed; container will start without this device");
                }
            }
        }

        let mut rt = self.rt.clone();
        let config = ContainerConfig {
            metadata: Some(ContainerMetadata { name: container.name.clone(), attempt }),
            image: Some(image_spec),
            command: container.command.clone().unwrap_or_default(),
            args: container.args.clone().unwrap_or_default(),
            working_dir: container.working_dir.clone().unwrap_or_default(),
            envs,
            mounts,
            devices,
            annotations,
            labels: container_labels(id, &container.name, kind),
            log_path: format!("{}_{}.log", container.name, attempt),
            linux,
            ..Default::default()
        };

        let created = match rt
            .create_container(CreateContainerRequest {
                pod_sandbox_id: sandbox_id.to_string(),
                config: Some(config),
                sandbox_config: Some(sandbox_config(id, None, &id.name, &HashMap::new())),
            })
            .await
        {
            Ok(resp) => resp.into_inner(),
            Err(e) => {
                self.release_devices(&allocated_devices);
                if let (Some(cpu_manager), Some(key)) = (&self.cpu_manager, &cpu_manager_key) {
                    cpu_manager.release(key);
                }
                if let (Some(memory_manager), Some(key)) = (&self.memory_manager, &memory_manager_key) {
                    memory_manager.release(key);
                }
                return Err(e).context("creating container");
            }
        };
        self.record_device_allocations(sandbox_id, &container.name, allocated_devices);

        if let Err(e) = rt.start_container(StartContainerRequest { container_id: created.container_id.clone() }).await {
            self.release_container_devices(sandbox_id, &container.name).await;
            return Err(e).context("starting container");
        }

        // Record this container's own resources so a later shared-pool
        // refresh (triggered by some *other* container's exclusive claim/
        // release) can find and update it, and — if this container itself
        // just took a new exclusive claim — sweep every other already-
        // running shared-pool container to exclude these cores now.
        let key = restart_count_key(sandbox_id, &container.name);
        self.container_resources.lock().unwrap().insert(key.clone(), (created.container_id.clone(), resources_for_record));
        self.applied_resources.lock().unwrap().insert(key, container.resources.clone().unwrap_or_default());
        if cpu_manager_key.is_some() {
            self.refresh_shared_pool_cpusets().await;
        }

        // postStart runs after the container is started; a failing hook
        // should kill+restart the container per real kubelet, but that's a
        // bigger behavior change than this pass takes on — logged and left
        // running instead (see docs/GAP_CLOSURE.md).
        if let Some(post_start) = container.lifecycle.as_ref().and_then(|l| l.post_start.as_ref()) {
            let pod_ip = self.pod_ip(sandbox_id).await.unwrap_or_default();
            if let Err(e) = self.run_lifecycle_hook(&created.container_id, &pod_ip, post_start, 30).await {
                warn!(container = %container.name, error = ?e, "postStart hook failed (container left running)");
            }
        }

        Ok(())
    }

    /// Run every currently-running app container's `preStop` hook (best
    /// effort — a failing hook must not block termination, matching real
    /// kubelet), then send each one `StopContainer` with the pod's
    /// termination grace period as the CRI timeout (containerd sends
    /// SIGTERM, waits up to that long, then SIGKILLs). Runs before
    /// `StopPodSandbox` so each container actually gets its own grace
    /// period instead of whatever the sandbox stop does by default.
    async fn graceful_stop_containers(&self, sandbox_id: &str, pod: &Pod, grace_seconds: i64) {
        let Ok(containers) = self.list_pod_containers(sandbox_id).await else { return };
        let running_v = ContainerState::ContainerRunning as i32;
        let pod_ip = self.pod_ip(sandbox_id).await.unwrap_or_default();
        // A still-*running* init-labeled container can only be a native
        // sidecar (round 36) — a regular init container blocks progression
        // until it exits, so it's never concurrently "running" alongside
        // this teardown path being reached. Sidecars get the same preStop +
        // graceful StopContainer treatment app containers do, so they're no
        // longer excluded here; their `preStop` hook (if any) lives on
        // `spec.initContainers`, not `spec.containers`, hence checking both.
        // **Simplification**: real kubelet stops sidecars strictly *after*
        // every app container has fully stopped; this stops everything in
        // one pass instead — not perfectly ordered, but every container
        // still gets its own graceful preStop + grace period.
        let spec_containers = pod.spec.as_ref().map(|s| s.containers.as_slice()).unwrap_or(&[]);
        let spec_init_containers = pod.spec.as_ref().and_then(|s| s.init_containers.as_deref()).unwrap_or(&[]);

        for c in &containers {
            if c.state != running_v {
                continue;
            }
            let Some(name) = c.labels.get(CTR_NAME_LABEL) else { continue };
            if let Some(pre_stop) = spec_containers
                .iter()
                .chain(spec_init_containers.iter())
                .find(|sc| &sc.name == name)
                .and_then(|sc| sc.lifecycle.as_ref())
                .and_then(|l| l.pre_stop.as_ref())
            {
                if let Err(e) = self.run_lifecycle_hook(&c.id, &pod_ip, pre_stop, grace_seconds).await {
                    warn!(container = %name, error = ?e, "preStop hook failed; continuing with termination anyway");
                }
            }
            let mut rt = self.rt.clone();
            let _ = rt.stop_container(StopContainerRequest { container_id: c.id.clone(), timeout: grace_seconds }).await;
        }
    }

    /// Execute one `postStart`/`preStop` handler. Supports `exec`, `httpGet`,
    /// and `sleep` (the newer preStop-only action) — not `tcpSocket` (the
    /// deprecated, rarely-used lifecycle hook form). Best-effort: errors are
    /// returned for the caller to log, never to block the container
    /// lifecycle transition that's waiting on this.
    async fn run_lifecycle_hook(
        &self,
        container_id: &str,
        pod_ip: &str,
        handler: &LifecycleHandler,
        timeout_secs: i64,
    ) -> Result<()> {
        let timeout = Duration::from_secs(timeout_secs.max(0) as u64);

        if let Some(exec) = &handler.exec {
            let command = exec.command.clone().unwrap_or_default();
            if command.is_empty() {
                return Ok(());
            }
            let mut rt = self.rt.clone();
            tokio::time::timeout(
                timeout,
                rt.exec_sync(ExecSyncRequest {
                    container_id: container_id.to_string(),
                    cmd: command,
                    timeout: timeout_secs.max(0),
                }),
            )
            .await
            .context("lifecycle hook exec timed out")?
            .context("lifecycle hook ExecSync")?;
            return Ok(());
        }

        if let Some(http) = &handler.http_get {
            let port = match &http.port {
                IntOrString::Int(n) => *n as u16,
                IntOrString::String(_) => 0, // named ports aren't resolvable here without the container spec; skip
            };
            let path = http.path.clone().unwrap_or_else(|| "/".to_string());
            let host = http.host.clone().filter(|h| !h.is_empty()).unwrap_or_else(|| pod_ip.to_string());
            // Best-effort — kubelet itself only logs a non-2xx/unreachable
            // lifecycle httpGet, it doesn't fail the container over it.
            let _ = tokio::time::timeout(timeout, lifecycle_http_get(&host, port, &path)).await;
            return Ok(());
        }

        if let Some(sleep) = &handler.sleep {
            tokio::time::sleep(Duration::from_secs(sleep.seconds.max(0) as u64).min(timeout)).await;
        }

        Ok(())
    }

    async fn container_exit_code(&self, container_id: &str) -> Result<i32> {
        Ok(self.container_status_details(container_id).await?.exit_code)
    }

    /// Full CRI `ContainerStatus` (exit code, reason, message, finished_at)
    /// for one container — the richer counterpart to `container_exit_code()`,
    /// used by `build_status()` (round 24) to populate `ContainerRuntimeStatus`'s
    /// terminated-state fields, not just decide pod phase.
    async fn container_status_details(&self, container_id: &str) -> Result<v1::ContainerStatus> {
        let mut rt = self.rt.clone();
        let resp = rt
            .container_status(ContainerStatusRequest { container_id: container_id.to_string(), verbose: false })
            .await
            .context("ContainerStatus")?
            .into_inner();
        resp.status.context("ContainerStatus response had no status")
    }

    /// Drive `spec.initContainers` one at a time, in order — exactly
    /// kubelet's sequencing: an init container must exit zero before the
    /// next one (or the app containers) starts. Each call advances at most
    /// one step (create-if-missing, or notice the front-of-line container
    /// finished) and reports where things stand; `ensure_pod()` calls this
    /// on every reconcile until it reports `AllComplete`.
    async fn ensure_init_containers(
        &self,
        sandbox_id: &str,
        id: &PodId,
        pod: &Pod,
        init_containers: &[Container],
        pod_sc: Option<&PodSecurityContext>,
        restart_policy: &str,
        volumes: &HashMap<String, ResolvedVolume>,
        pull_secrets: &[String],
        service_env: &BTreeMap<String, Vec<u8>>,
        qos: QosClass,
    ) -> Result<InitProgress> {
        let running_v = ContainerState::ContainerRunning as i32;
        let exited_v = ContainerState::ContainerExited as i32;
        let existing = self.list_pod_containers(sandbox_id).await?;

        for container in init_containers {
            let existing_ctr = existing.iter().find(|c| {
                c.labels.get(CTR_NAME_LABEL).map(|n| n == &container.name).unwrap_or(false)
                    && c.labels.get(CTR_INIT_LABEL).map(|v| v == "true").unwrap_or(false)
            });

            // Native sidecar container (round 36):
            // initContainers[].restartPolicy == "Always". Unlike a regular
            // init container, this doesn't block later init/app containers
            // on its own *exit* — only on having been started at all — and
            // it restarts on exit like a normal container for the pod's
            // whole lifetime, handled right here on every reconcile rather
            // than through the app-container restart path (it's reported
            // under initContainerStatuses, not containerStatuses, matching
            // upstream).
            if container.restart_policy.as_deref() == Some("Always") {
                match sidecar_init_decision(existing_ctr.map(|c| c.state), running_v, exited_v) {
                    SidecarInitDecision::Create => {
                        let envs = self.resolve_container_env(pod, id, container, service_env).await?;
                        let attempt = self.restart_count(sandbox_id, &container.name);
                        self.create_and_start_container(
                            sandbox_id, id, container, pod_sc, volumes, pull_secrets, &envs, ContainerKind::Init, attempt, qos,
                        )
                        .await?;
                        // Gate later containers on this one actually
                        // starting — the next reconcile (triggered by the
                        // CRI event once it's running) picks up past it.
                        return Ok(InitProgress::Waiting);
                    }
                    SidecarInitDecision::NeedsRestart => {
                        // Exited — restart it, but don't block the rest of
                        // the sequence on this restart; later containers
                        // already saw it start once.
                        let c = existing_ctr.expect("NeedsRestart only reached when a container exists");
                        self.bump_restart_count(sandbox_id, &container.name);
                        self.release_container_devices(sandbox_id, &container.name).await;
                        let mut rt = self.rt.clone();
                        let _ = rt.remove_container(RemoveContainerRequest { container_id: c.id.clone() }).await;
                        continue;
                    }
                    SidecarInitDecision::Started => continue,
                }
            }

            let exit_code = match existing_ctr {
                Some(c) if c.state == exited_v => self.container_exit_code(&c.id).await?,
                _ => 0,
            };

            match init_container_decision(existing_ctr.map(|c| c.state), running_v, exited_v, exit_code, restart_policy) {
                InitContainerDecision::Create => {
                    let envs = self.resolve_container_env(pod, id, container, service_env).await?;
                    let attempt = self.restart_count(sandbox_id, &container.name);
                    self.create_and_start_container(
                        sandbox_id, id, container, pod_sc, volumes, pull_secrets, &envs, ContainerKind::Init, attempt, qos,
                    )
                    .await?;
                    return Ok(InitProgress::Waiting);
                }
                InitContainerDecision::Done => continue, // this init container is done — check the next one
                InitContainerDecision::Failed => {
                    return Ok(InitProgress::Failed(format!(
                        "init container {} exited with code {exit_code}",
                        container.name
                    )));
                }
                InitContainerDecision::Retry => {
                    // Allowed to retry — clear it out; the next reconcile
                    // (triggered by this very removal, via the CRI event
                    // stream) sees no existing container and creates a fresh one.
                    let c = existing_ctr.expect("Retry only reached when a container exists");
                    self.bump_restart_count(sandbox_id, &container.name);
                    self.release_container_devices(sandbox_id, &container.name).await;
                    let mut rt = self.rt.clone();
                    let _ = rt.remove_container(RemoveContainerRequest { container_id: c.id.clone() }).await;
                    return Ok(InitProgress::Waiting);
                }
                InitContainerDecision::StillRunning | InitContainerDecision::Waiting => return Ok(InitProgress::Waiting),
            }
        }
        Ok(InitProgress::AllComplete)
    }

    /// Every nodelet-managed sandbox on the node, unfiltered by pod — the
    /// `find_sandbox()` lookups elsewhere always scope to one pod; GC needs
    /// the reverse view (every sandbox, checked against the apiserver).
    async fn list_all_sandboxes(&self) -> Result<Vec<(String, String, String)>> {
        let mut rt = self.rt.clone();
        let resp = rt.list_pod_sandbox(ListPodSandboxRequest { filter: None }).await?.into_inner();
        Ok(resp
            .items
            .into_iter()
            .filter_map(|s| Some((s.labels.get(POD_NS_LABEL)?.clone(), s.labels.get(POD_NAME_LABEL)?.clone(), s.id)))
            .collect())
    }

    async fn gc_orphaned_sandboxes(&self, live_pod_keys: &HashSet<String>) -> Result<()> {
        let sandboxes = self.list_all_sandboxes().await?;
        let orphans = crate::gc::orphaned_sandboxes(&sandboxes, live_pod_keys);
        for sandbox_id in orphans {
            info!(sandbox = %sandbox_id, "gc: removing orphaned sandbox (pod no longer in apiserver)");
            let mut rt = self.rt.clone();
            let _ = rt.stop_pod_sandbox(StopPodSandboxRequest { pod_sandbox_id: sandbox_id.clone() }).await;
            if let Err(e) = rt.remove_pod_sandbox(RemovePodSandboxRequest { pod_sandbox_id: sandbox_id.clone() }).await {
                warn!(sandbox = %sandbox_id, error = ?e, "gc: failed to remove orphaned sandbox");
            }
            self.restart_policies.lock().unwrap().remove(&sandbox_id);
            if let Some(pod_uid) = self.pod_uids.lock().unwrap().remove(&sandbox_id) {
                self.userns.release(&pod_uid);
            }
            self.sidecar_names.lock().unwrap().remove(&sandbox_id);
            self.clear_restart_counts(&sandbox_id);
            self.release_sandbox_devices(&sandbox_id).await;
        }
        Ok(())
    }

    async fn gc_unreferenced_images(&self) -> Result<()> {
        let mut rt = self.rt.clone();
        let containers = rt
            .list_containers(ListContainersRequest { filter: None })
            .await?
            .into_inner()
            .containers;
        let referenced: HashSet<String> = containers
            .into_iter()
            .filter_map(|c| c.image.map(|i| i.image))
            .collect();

        let mut img = self.img.clone();
        let images = img.list_images(ListImagesRequest { filter: None }).await?.into_inner().images;
        let refs: Vec<crate::gc::ImageRef> = images
            .into_iter()
            .map(|i| crate::gc::ImageRef { id: i.id, repo_tags: i.repo_tags, repo_digests: i.repo_digests })
            .collect();

        for image_id in crate::gc::images_to_gc(&refs, &referenced) {
            info!(image = %image_id, "gc: removing unreferenced image");
            let image_spec = ImageSpec { image: image_id.clone(), ..Default::default() };
            if let Err(e) = img.remove_image(RemoveImageRequest { image: Some(image_spec) }).await {
                warn!(image = %image_id, error = ?e, "gc: failed to remove unreferenced image");
            }
        }
        Ok(())
    }

    async fn pod_ip(&self, sandbox_id: &str) -> Option<String> {
        let mut rt = self.rt.clone();
        let resp = rt
            .pod_sandbox_status(PodSandboxStatusRequest {
                pod_sandbox_id: sandbox_id.to_string(),
                verbose: false,
            })
            .await
            .ok()?
            .into_inner();
        let ip = resp.status?.network?.ip;
        (!ip.is_empty()).then_some(ip)
    }

    async fn build_status(&self, sandbox_id: &str, pod_uid: &str, restart_policy: &str) -> Result<RuntimeStatus> {
        // Init containers are excluded here — by the time app containers are
        // even started, every init container has already exited zero
        // (ensure_init_containers() gates on that), so counting them would
        // make `all_exited` true for entirely the wrong reason.
        let containers: Vec<_> = self
            .list_pod_containers(sandbox_id)
            .await?
            .into_iter()
            .filter(|c| !c.labels.contains_key(CTR_INIT_LABEL) && !c.labels.contains_key(CTR_EPHEMERAL_LABEL))
            .collect();
        let running_v = ContainerState::ContainerRunning as i32;
        let exited_v = ContainerState::ContainerExited as i32;

        let mut crs = Vec::new();
        let mut any_running = false;
        let mut all_exited = !containers.is_empty();
        // Unlike the old exit-code-only check this replaces (which only
        // ever ran for restartPolicy: Never, and only once every container
        // had exited), terminated-state details below are now fetched for
        // *any* individual exited container regardless of restart policy or
        // sibling state — real value for a crash-looping Always-restart
        // container (kubectl describe should show *why* it last died), not
        // just Job-style completion. Still bounded to "no longer running"
        // containers only, so a healthy steady-state pod pays zero extra
        // RPCs, matching this codebase's low-idle-cost design throughout.
        let mut any_failed = false;
        let mut earliest_created = i64::MAX;

        for c in &containers {
            let running = c.state == running_v;
            let exited = c.state == exited_v;
            any_running |= running;
            all_exited &= exited;
            earliest_created = earliest_created.min(c.created_at);
            let name = c.metadata.as_ref().map(|m| m.name.clone()).unwrap_or_default();

            let (exit_code, reason, finished_at, termination_message) = if exited {
                match self.container_status_details(&c.id).await {
                    Ok(details) => {
                        if details.exit_code != 0 {
                            any_failed = true;
                        }
                        let reason = if !details.reason.is_empty() {
                            details.reason
                        } else if details.exit_code == 0 {
                            "Completed".to_string()
                        } else {
                            "Error".to_string()
                        };
                        let finished_at = (details.finished_at > 0)
                            .then(|| Timestamp::from_nanosecond(details.finished_at as i128).ok())
                            .flatten();
                        let message = read_termination_message(&termination_message_host_path(pod_uid, &name));
                        (Some(details.exit_code), reason, finished_at, message)
                    }
                    Err(e) => {
                        warn!(container = %c.id, error = ?e, "ContainerStatus failed; reporting this exited container without terminated details");
                        (None, String::new(), None, String::new())
                    }
                }
            } else {
                (None, String::new(), None, String::new())
            };

            let resource_key = restart_count_key(sandbox_id, &name);
            let resources = self.applied_resources.lock().unwrap().get(&resource_key).cloned();
            let allocated_resources = self.spec_resources.lock().unwrap().get(&resource_key).cloned();

            crs.push(ContainerRuntimeStatus {
                name: name.clone(),
                image: c.image.as_ref().map(|i| i.image.clone()).unwrap_or_default(),
                image_id: c.image_ref.clone(),
                ready: running,
                running,
                container_id: Some(format_container_id(&self.runtime_name, &c.id)),
                restart_count: self.restart_count(sandbox_id, &name),
                exit_code,
                reason,
                finished_at,
                termination_message,
                is_restartable_sidecar: false, // app containers, never a sidecar concept
                resources,
                allocated_resources,
            });
        }

        // The pod's phase must only treat a nonzero exit as terminal
        // failure under restartPolicy: Never — Always/OnFailure exits
        // aren't final, ensure_container() above just restarted them.
        let phase_failed = any_failed && all_exited && restart_policy == "Never";
        let phase = compute_phase(any_running, all_exited, phase_failed, restart_policy);

        let started_at = (earliest_created != i64::MAX && earliest_created > 0)
            .then(|| Timestamp::from_nanosecond(earliest_created as i128).ok())
            .flatten();

        Ok(RuntimeStatus {
            phase,
            message: None,
            started_at,
            pod_ip: self.pod_ip(sandbox_id).await,
            containers: crs,
            init_containers: self.build_labeled_container_statuses(sandbox_id, pod_uid, CTR_INIT_LABEL, true).await.unwrap_or_default(),
            ephemeral_containers: self
                .build_labeled_container_statuses(sandbox_id, pod_uid, CTR_EPHEMERAL_LABEL, false)
                .await
                .unwrap_or_default(),
            initialized: true,
        })
    }

    /// `ContainerRuntimeStatus` for every container in the sandbox carrying
    /// `label` (either `CTR_INIT_LABEL` or `CTR_EPHEMERAL_LABEL`) — the
    /// counterpart to the main loop in `build_status()` above, kept separate
    /// because both init and ephemeral containers are excluded from that one
    /// (their exit is expected/irrelevant, not something `all_exited` should
    /// key the *pod's* phase off). `fetch_details` gates the same
    /// terminated-state enrichment `build_status()`'s main loop does — `true`
    /// for init containers (a failed init container's exit reason matters,
    /// same as app containers), `false` for ephemeral/debug containers
    /// (round 8's existing, still-documented simplification: exit codes
    /// aren't tracked for those at all, `pods.rs` hardcodes `exit_code: 0`
    /// regardless of what's fetched here, so there's no reason to pay the
    /// extra `ContainerStatus` RPC for them).
    async fn build_labeled_container_statuses(
        &self,
        sandbox_id: &str,
        pod_uid: &str,
        label: &str,
        fetch_details: bool,
    ) -> Result<Vec<ContainerRuntimeStatus>> {
        let running_v = ContainerState::ContainerRunning as i32;
        let exited_v = ContainerState::ContainerExited as i32;
        let containers = self.list_pod_containers(sandbox_id).await?;
        let sidecar_names = self.sidecar_names.lock().unwrap().get(sandbox_id).cloned().unwrap_or_default();
        let mut out = Vec::new();
        for c in containers.into_iter().filter(|c| c.labels.contains_key(label)) {
                let running = c.state == running_v;
                let name = c.metadata.as_ref().map(|m| m.name.clone()).unwrap_or_default();
                let is_restartable_sidecar = sidecar_names.contains(&name);
                let (exit_code, reason, finished_at, termination_message) = if fetch_details && c.state == exited_v {
                    match self.container_status_details(&c.id).await {
                        Ok(details) => {
                            let reason = if !details.reason.is_empty() {
                                details.reason
                            } else if details.exit_code == 0 {
                                "Completed".to_string()
                            } else {
                                "Error".to_string()
                            };
                            let finished_at = (details.finished_at > 0)
                                .then(|| Timestamp::from_nanosecond(details.finished_at as i128).ok())
                                .flatten();
                            let message = read_termination_message(&termination_message_host_path(pod_uid, &name));
                            (Some(details.exit_code), reason, finished_at, message)
                        }
                        Err(e) => {
                            warn!(container = %c.id, error = ?e, "ContainerStatus failed; reporting this exited container without terminated details");
                            (None, String::new(), None, String::new())
                        }
                    }
                } else {
                    (None, String::new(), None, String::new())
                };
                out.push(ContainerRuntimeStatus {
                    restart_count: self.restart_count(sandbox_id, &name),
                    name,
                    image: c.image.as_ref().map(|i| i.image.clone()).unwrap_or_default(),
                    image_id: c.image_ref.clone(),
                    ready: running,
                    running,
                    container_id: Some(format_container_id(&self.runtime_name, &c.id)),
                    exit_code,
                    reason,
                    finished_at,
                    termination_message,
                    is_restartable_sidecar,
                    // Resize status reporting (round 43) is scoped to app
                    // containers only this round — init/ephemeral containers
                    // don't get a resize decision at all yet either.
                    resources: None,
                    allocated_resources: None,
                });
        }
        Ok(out)
    }
}

#[async_trait]
impl PodRuntime for CriRuntime {
    async fn ensure_pod(&self, pod: &Pod) -> Result<RuntimeStatus> {
        let id = pod_id(pod);
        let found = self.find_sandbox(&id.namespace, &id.name).await?;
        let ready_state = v1::PodSandboxState::SandboxReady as i32;
        let dns = dns_config_for(pod, &self.cluster_dns, &self.cluster_domain);
        let runtime_handler = self.resolve_runtime_handler(pod).await;
        let spec = pod.spec.as_ref();
        let hostname = resolve_pod_hostname(
            spec.and_then(|s| s.hostname.as_deref()),
            spec.and_then(|s| s.subdomain.as_deref()),
            spec.and_then(|s| s.set_hostname_as_fqdn).unwrap_or(false),
            &id.name,
            &id.namespace,
            &self.cluster_domain,
        )?;
        let sysctls = pod_sysctls(spec.and_then(|s| s.security_context.as_ref()));
        // Real kubelet computes both of these before RunPodSandbox too —
        // cgroup_parent so the sandbox lands in the right QoS-scoped cgroup
        // from the start (not something CRI lets you change after the
        // fact), overhead so a RuntimeClass with declared overhead
        // (gVisor/Kata's userspace kernel cost) gets it accounted into the
        // sandbox's own resources.
        let qos = crate::eviction::qos_class(pod);
        let cgroup_parent = crate::cgroup::cgroup_parent_for(qos, &id.uid);
        let overhead = pod.spec.as_ref().and_then(|s| s.overhead.as_ref()).map(|list| resource_list_to_linux_resources(list));
        let sandbox_id = match sandbox_reuse_decision(found.as_ref().map(|(_, s)| *s), ready_state) {
            SandboxDecision::Reuse => found.unwrap().0,
            SandboxDecision::RecreateStale => {
                // The sandbox record exists but its task/pause process
                // isn't alive (e.g. this metadata survived a reboot but
                // the process didn't) — tear it down and start clean
                // instead of reusing something CreateContainer can never
                // succeed against. Best-effort: it may already be half-gone.
                let (stale_id, _) = found.unwrap();
                let mut rt = self.rt.clone();
                let _ = rt.stop_pod_sandbox(StopPodSandboxRequest { pod_sandbox_id: stale_id.clone() }).await;
                let _ = rt.remove_pod_sandbox(RemovePodSandboxRequest { pod_sandbox_id: stale_id.clone() }).await;
                self.restart_policies.lock().unwrap().remove(&stale_id);
                self.pod_uids.lock().unwrap().remove(&stale_id);
                self.sidecar_names.lock().unwrap().remove(&stale_id);
                self.clear_restart_counts(&stale_id);
                self.release_sandbox_devices(&stale_id).await;
                self.run_sandbox(&id, &hostname, &sysctls, dns, runtime_handler, cgroup_parent, overhead).await.context("RunPodSandbox")?
            }
            SandboxDecision::CreateFresh => {
                self.run_sandbox(&id, &hostname, &sysctls, dns, runtime_handler, cgroup_parent, overhead).await.context("RunPodSandbox")?
            }
        };

        let restart_policy = pod
            .spec
            .as_ref()
            .and_then(|s| s.restart_policy.clone())
            .unwrap_or_else(|| "Always".to_string());
        // Recorded for status()'s event-driven path, which only gets
        // namespace+name (no Pod object) and needs this to make the same
        // Pending-vs-Succeeded call build_status() below does.
        self.restart_policies.lock().unwrap().insert(sandbox_id.clone(), restart_policy.clone());
        self.pod_uids.lock().unwrap().insert(sandbox_id.clone(), id.uid.clone());
        // Native sidecar containers (round 36): initContainers[].restartPolicy
        // == "Always". Recorded so build_labeled_container_statuses() (whose
        // event-driven callers have no Pod object) knows which init
        // containers should get real probe-based readiness folded into the
        // pod's overall Ready/ContainersReady, same as app containers.
        let sidecar_names: HashSet<String> = pod
            .spec
            .as_ref()
            .and_then(|s| s.init_containers.as_ref())
            .map(|cs| cs.iter().filter(|c| c.restart_policy.as_deref() == Some("Always")).map(|c| c.name.clone()).collect())
            .unwrap_or_default();
        self.sidecar_names.lock().unwrap().insert(sandbox_id.clone(), sidecar_names);

        let pod_sc = pod.spec.as_ref().and_then(|s| s.security_context.as_ref());
        let pull_secrets: Vec<String> = pod
            .spec
            .as_ref()
            .and_then(|s| s.image_pull_secrets.as_ref())
            .map(|refs| refs.iter().map(|r| r.name.clone()).filter(|n| !n.is_empty()).collect())
            .unwrap_or_default();

        let volumes = self.resolve_volumes(pod, &id, &pull_secrets).await;
        let service_env = if pod.spec.as_ref().and_then(|s| s.enable_service_links) == Some(false) {
            BTreeMap::new()
        } else {
            self.resolve_service_env(&id.namespace)
                .await
                .context("resolving Service environment")?
        };

        let init_containers = pod.spec.as_ref().and_then(|s| s.init_containers.clone()).unwrap_or_default();
        if !init_containers.is_empty() {
            let progress = self
                .ensure_init_containers(
                    &sandbox_id,
                    &id,
                    pod,
                    &init_containers,
                    pod_sc,
                    &restart_policy,
                    &volumes,
                    &pull_secrets,
                    &service_env,
                    qos,
                )
                .await?;
            match progress {
                InitProgress::Waiting => {
                    return Ok(RuntimeStatus {
                        phase: Phase::Pending,
                        message: Some("waiting for init containers to complete".to_string()),
                        started_at: None,
                        pod_ip: None,
                        containers: Vec::new(),
                        init_containers: self
                            .build_labeled_container_statuses(&sandbox_id, &id.uid, CTR_INIT_LABEL, true)
                            .await
                            .unwrap_or_default(),
                        ephemeral_containers: Vec::new(),
                        initialized: false,
                    });
                }
                InitProgress::Failed(reason) => {
                    return Ok(RuntimeStatus {
                        phase: Phase::Failed,
                        message: Some(reason),
                        started_at: None,
                        pod_ip: None,
                        containers: Vec::new(),
                        init_containers: self
                            .build_labeled_container_statuses(&sandbox_id, &id.uid, CTR_INIT_LABEL, true)
                            .await
                            .unwrap_or_default(),
                        ephemeral_containers: Vec::new(),
                        initialized: false,
                    });
                }
                InitProgress::AllComplete => {}
            }
        }

        if let Some(spec) = pod.spec.as_ref() {
            for c in &spec.containers {
                let envs = self.resolve_container_env(pod, &id, c, &service_env).await?;
                self.ensure_container(&sandbox_id, &id, c, pod_sc, &restart_policy, &volumes, &pull_secrets, &envs, qos)
                    .await?;
            }
            // Added post-hoc via the `ephemeralcontainers` subresource (e.g.
            // `kubectl debug`), never present when a pod is first created —
            // best-effort: a failure to start one debug container shouldn't
            // fail reconciling the rest of a pod that's otherwise healthy.
            for ec in spec.ephemeral_containers.as_deref().unwrap_or(&[]) {
                let container = ephemeral_to_container(ec);
                if let Err(e) = self
                    .ensure_ephemeral_container(&sandbox_id, &id, pod, &container, pod_sc, &volumes, &pull_secrets, &service_env)
                    .await
                {
                    warn!(container = %ec.name, error = ?e, "failed to start ephemeral container");
                }
            }
        }

        self.build_status(&sandbox_id, &id.uid, &restart_policy).await
    }

    async fn remove_pod(&self, pod: &Pod) -> Result<()> {
        let id = pod_id(pod);
        if let Some((sandbox_id, _state)) = self.find_sandbox(&id.namespace, &id.name).await? {
            let grace = termination_grace_seconds(pod);
            self.graceful_stop_containers(&sandbox_id, pod, grace).await;

            let mut rt = self.rt.clone();
            // StopPodSandbox is idempotent; RemovePodSandbox also removes its containers.
            let _ = rt
                .stop_pod_sandbox(StopPodSandboxRequest { pod_sandbox_id: sandbox_id.clone() })
                .await;
            rt.remove_pod_sandbox(RemovePodSandboxRequest { pod_sandbox_id: sandbox_id.clone() })
                .await
                .context("RemovePodSandbox")?;
            self.restart_policies.lock().unwrap().remove(&sandbox_id);
            if let Some(pod_uid) = self.pod_uids.lock().unwrap().remove(&sandbox_id) {
                self.userns.release(&pod_uid);
            }
            self.sidecar_names.lock().unwrap().remove(&sandbox_id);
            self.clear_restart_counts(&sandbox_id);
            self.release_sandbox_devices(&sandbox_id).await;
        }
        self.unmount_csi_volumes(pod, &id).await;
        unmount_memory_backed_empty_dirs(pod, &id);
        Ok(())
    }

    async fn status(&self, namespace: &str, name: &str) -> Result<Option<RuntimeStatus>> {
        match self.find_sandbox(namespace, name).await? {
            Some((sandbox_id, _state)) => {
                let restart_policy = self
                    .restart_policies
                    .lock()
                    .unwrap()
                    .get(&sandbox_id)
                    .cloned()
                    .unwrap_or_else(|| "Always".to_string());
                let pod_uid = self.pod_uids.lock().unwrap().get(&sandbox_id).cloned().unwrap_or_default();
                Ok(Some(self.build_status(&sandbox_id, &pod_uid, &restart_policy).await?))
            }
            None => Ok(None),
        }
    }

    fn take_event_rx(&self) -> Option<UnboundedReceiver<String>> {
        self.rx.lock().unwrap().take()
    }

    async fn exec(&self, namespace: &str, name: &str, container: &str, command: &[String]) -> Result<bool> {
        let Some((sandbox_id, _)) = self.find_sandbox(namespace, name).await? else {
            return Ok(false); // pod gone; nothing to exec into
        };
        let Some(container_id) = self.find_container_id(&sandbox_id, container).await? else {
            return Ok(false); // container gone (mid-restart); treat as a failed probe, not an error
        };
        let mut rt = self.rt.clone();
        let resp = rt
            .exec_sync(ExecSyncRequest {
                container_id,
                cmd: command.to_vec(),
                timeout: 0, // caller (probes.rs) already bounds this with its own tokio::time::timeout
            })
            .await
            .context("ExecSync")?
            .into_inner();
        Ok(resp.exit_code == 0)
    }

    async fn restart_container(&self, namespace: &str, name: &str, container: &str, grace_period_seconds: i64) -> Result<()> {
        let Some((sandbox_id, _)) = self.find_sandbox(namespace, name).await? else {
            return Ok(()); // pod already gone; nothing to restart
        };
        let Some(container_id) = self.find_container_id(&sandbox_id, container).await? else {
            return Ok(()); // already gone; the next ensure_pod() will create it fresh anyway
        };
        let mut rt = self.rt.clone();
        // Best-effort stop before remove: a container that's still alive
        // (this is a *liveness* failure, not necessarily a crash) needs to
        // actually be killed, not just have its CRI record dropped.
        // `grace_period_seconds` (round 44) honors the probe's own
        // `terminationGracePeriodSeconds` override if set, else the pod's
        // own — previously this was always a hardcoded 10s regardless of
        // either.
        let _ = rt
            .stop_container(StopContainerRequest { container_id: container_id.clone(), timeout: grace_period_seconds.max(0) })
            .await;
        rt.remove_container(RemoveContainerRequest { container_id }).await.context("RemoveContainerRequest")?;
        Ok(())
    }

    async fn gc(&self, live_pod_keys: &HashSet<String>) -> Result<()> {
        self.gc_orphaned_sandboxes(live_pod_keys).await?;
        self.gc_unreferenced_images().await?;
        Ok(())
    }

    async fn rotate_logs(&self, max_size_bytes: u64, max_files: u32) -> Result<()> {
        let running_v = ContainerState::ContainerRunning as i32;
        let mut rt = self.rt.clone();
        let containers = rt
            .list_containers(ListContainersRequest { filter: None })
            .await
            .context("ListContainers")?
            .into_inner()
            .containers;

        for c in containers {
            if c.state != running_v {
                continue; // nothing actively writing to a stopped container's log
            }
            let status = match rt.container_status(ContainerStatusRequest { container_id: c.id.clone(), verbose: false }).await {
                Ok(resp) => resp.into_inner().status,
                Err(e) => {
                    warn!(container = %c.id, error = ?e, "log rotation: ContainerStatus failed");
                    continue;
                }
            };
            let Some(log_path) = status.map(|s| s.log_path).filter(|p| !p.is_empty()) else { continue };
            let size = match std::fs::metadata(&log_path) {
                Ok(meta) => meta.len(),
                Err(_) => continue, // no log file yet, or already rotated by a previous tick
            };
            if size <= max_size_bytes {
                continue;
            }

            if let Err(e) = rotate_log_file(std::path::Path::new(&log_path), max_files) {
                warn!(container = %c.id, log_path, error = ?e, "log rotation: failed to rotate log file");
                continue;
            }
            // Tell the runtime to reopen its fd at the same path — after the
            // rename above, its existing fd would otherwise keep writing to
            // the now-renamed old file forever.
            if let Err(e) = rt.reopen_container_log(ReopenContainerLogRequest { container_id: c.id.clone() }).await {
                warn!(container = %c.id, error = ?e, "log rotation: ReopenContainerLog failed");
            }
        }
        Ok(())
    }

    async fn container_log_path(&self, namespace: &str, name: &str, container: &str) -> Result<Option<String>> {
        let Some((sandbox_id, _)) = self.find_sandbox(namespace, name).await? else { return Ok(None) };
        let Some(container_id) = self.find_container_id(&sandbox_id, container).await? else { return Ok(None) };
        let mut rt = self.rt.clone();
        let status = rt
            .container_status(ContainerStatusRequest { container_id, verbose: false })
            .await
            .context("ContainerStatus")?
            .into_inner()
            .status;
        Ok(status.map(|s| s.log_path).filter(|p| !p.is_empty()))
    }

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
    ) -> Result<String> {
        let Some((sandbox_id, _)) = self.find_sandbox(namespace, name).await? else {
            anyhow::bail!("pod {namespace}/{name} not found")
        };
        let Some(container_id) = self.find_container_id(&sandbox_id, container).await? else {
            anyhow::bail!("container {container} not found in pod {namespace}/{name}")
        };
        let mut rt = self.rt.clone();
        let resp = rt
            .exec(ExecRequest { container_id, cmd: cmd.to_vec(), tty, stdin, stdout, stderr })
            .await
            .context("Exec")?
            .into_inner();
        Ok(resp.url)
    }

    async fn attach_url(
        &self,
        namespace: &str,
        name: &str,
        container: &str,
        stdin: bool,
        stdout: bool,
        stderr: bool,
        tty: bool,
    ) -> Result<String> {
        let Some((sandbox_id, _)) = self.find_sandbox(namespace, name).await? else {
            anyhow::bail!("pod {namespace}/{name} not found")
        };
        let Some(container_id) = self.find_container_id(&sandbox_id, container).await? else {
            anyhow::bail!("container {container} not found in pod {namespace}/{name}")
        };
        let mut rt = self.rt.clone();
        let resp = rt
            .attach(AttachRequest { container_id, stdin, tty, stdout, stderr })
            .await
            .context("Attach")?
            .into_inner();
        Ok(resp.url)
    }

    async fn port_forward_url(&self, namespace: &str, name: &str) -> Result<String> {
        let Some((sandbox_id, _)) = self.find_sandbox(namespace, name).await? else {
            anyhow::bail!("pod {namespace}/{name} not found")
        };
        let mut rt = self.rt.clone();
        let resp = rt
            .port_forward(PortForwardRequest { pod_sandbox_id: sandbox_id, port: Vec::new() })
            .await
            .context("PortForward")?
            .into_inner();
        Ok(resp.url)
    }

    async fn pod_usage_stats(&self) -> Result<Vec<super::PodUsage>> {
        let mut rt = self.rt.clone();
        let stats = rt
            .list_pod_sandbox_stats(ListPodSandboxStatsRequest { filter: None })
            .await
            .context("ListPodSandboxStats")?
            .into_inner()
            .stats;
        Ok(stats.iter().filter_map(pod_usage_from_sandbox_stats).collect())
    }

    fn device_plugin_capacity(&self) -> BTreeMap<String, u64> {
        self.device_plugins.capacity_map()
    }

    async fn node_images(&self) -> Result<Vec<crate::runtime::NodeImage>> {
        let mut img = self.img.clone();
        let images = img.list_images(ListImagesRequest { filter: None }).await?.into_inner().images;
        Ok(images.into_iter().map(node_image_from_cri).collect())
    }

    fn mounted_csi_volumes(&self) -> Vec<(String, String)> {
        self.csi.mounted_volumes()
    }

    async fn runtime_handlers(&self) -> Result<Vec<crate::runtime::RuntimeHandlerInfo>> {
        let mut rt = self.rt.clone();
        let handlers = rt.status(StatusRequest { verbose: false }).await?.into_inner().runtime_handlers;
        Ok(handlers.into_iter().map(runtime_handler_from_cri).collect())
    }
}

/// CRI's `Image` (`repo_tags`/`repo_digests`/`size`) -> `Node.status.images`'
/// shape (`names` combining both, `size_bytes`) — pure so the combination
/// is unit-testable without a real image cache.
fn node_image_from_cri(image: v1::Image) -> crate::runtime::NodeImage {
    let mut names = image.repo_tags;
    names.extend(image.repo_digests);
    crate::runtime::NodeImage { names, size_bytes: image.size }
}

/// Real kubelet's `<runtimeName>://<id>` container-ID format (round 57;
/// found in round 54's re-audit) — applied to `ContainerRuntimeStatus.container_id`
/// right where it's populated from CRI's own bare ID, so every downstream
/// consumer (`ContainerStatus.containerID`, `state.terminated.containerID`
/// — both read the same field) gets the prefix without needing its own
/// formatting logic.
fn format_container_id(runtime_name: &str, id: &str) -> String {
    format!("{runtime_name}://{id}")
}

/// CRI's `RuntimeHandler` -> `Node.status.runtimeHandlers`' shape (round
/// 53) — pure so the field mapping is unit-testable without a real CRI
/// socket.
fn runtime_handler_from_cri(h: v1::RuntimeHandler) -> crate::runtime::RuntimeHandlerInfo {
    let features = h.features.unwrap_or_default();
    crate::runtime::RuntimeHandlerInfo {
        name: h.name,
        recursive_read_only_mounts: features.recursive_read_only_mounts,
        user_namespaces: features.user_namespaces,
    }
}

fn u64_value(v: &Option<v1::UInt64Value>) -> Option<u64> {
    v.as_ref().map(|v| v.value)
}

fn usage_stats_from_cpu_memory(cpu: Option<&v1::CpuUsage>, memory: Option<&v1::MemoryUsage>) -> super::UsageStats {
    super::UsageStats {
        cpu_usage_nano_cores: cpu.and_then(|c| u64_value(&c.usage_nano_cores)),
        cpu_usage_core_nano_seconds: cpu.and_then(|c| u64_value(&c.usage_core_nano_seconds)),
        memory_working_set_bytes: memory.and_then(|m| u64_value(&m.working_set_bytes)),
        memory_usage_bytes: memory.and_then(|m| u64_value(&m.usage_bytes)),
        memory_rss_bytes: memory.and_then(|m| u64_value(&m.rss_bytes)),
        memory_available_bytes: memory.and_then(|m| u64_value(&m.available_bytes)),
    }
}

/// Sum of every container's CRI-reported writable-layer usage (round 49)
/// — the container's own filesystem writes (anything not on a mounted
/// volume), the piece of ephemeral-storage usage nodelet has no other way
/// to measure itself (containerd owns that storage, not nodelet).
fn writable_layer_bytes(containers: &[v1::ContainerStats]) -> u64 {
    containers.iter().filter_map(|c| c.writable_layer.as_ref().and_then(|w| u64_value(&w.used_bytes))).sum()
}

/// Recursive directory size, in bytes — nodelet's own materialized volume
/// directory (round 49) is the other half of a pod's ephemeral-storage
/// usage: emptyDir/ConfigMap/Secret/downwardAPI/projected content nodelet
/// itself writes, which containerd's own stats never account for since
/// they're not part of any container's writable layer. `0` on any error
/// (missing directory — most pods have no such volumes at all — or a
/// permission issue), matching this file's existing "best-effort,
/// fail-open" posture for filesystem reads.
fn directory_usage_bytes(path: &std::path::Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(path) else { return 0 };
    let mut total = 0u64;
    for entry in entries.flatten() {
        match entry.metadata() {
            Ok(meta) if meta.is_dir() => total += directory_usage_bytes(&entry.path()),
            Ok(meta) => total += meta.len(),
            Err(_) => {}
        }
    }
    total
}

/// Convert one CRI `PodSandboxStats` into nodelet's runtime-agnostic
/// `PodUsage`. `None` if the sandbox has no identifying metadata or no
/// Linux stats attached (Windows stats, or a sandbox CRI hasn't measured
/// yet) — nothing meaningful to report either way.
fn pod_usage_from_sandbox_stats(stats: &v1::PodSandboxStats) -> Option<super::PodUsage> {
    let attrs = stats.attributes.as_ref()?;
    let metadata = attrs.metadata.as_ref()?;
    let linux = stats.linux.as_ref()?;

    let containers = linux
        .containers
        .iter()
        .filter_map(|c| {
            let name = c.attributes.as_ref()?.metadata.as_ref()?.name.clone();
            Some(super::ContainerUsage {
                name,
                stats: usage_stats_from_cpu_memory(c.cpu.as_ref(), c.memory.as_ref()),
            })
        })
        .collect();

    // Local ephemeral storage (round 49): container writable layers
    // (containerd's own stats) plus nodelet's own materialized volume
    // directory (not part of any writable layer). Known scope
    // limitation: doesn't include container log file size
    // (/var/log/pods/...) — see PodUsage's own doc comment.
    let volume_dir = PathBuf::from(VOLUME_ROOT).join(&metadata.uid).join("volumes");
    let ephemeral_storage_usage_bytes =
        Some(writable_layer_bytes(&linux.containers) + directory_usage_bytes(&volume_dir));

    Some(super::PodUsage {
        namespace: metadata.namespace.clone(),
        name: metadata.name.clone(),
        uid: metadata.uid.clone(),
        pod: usage_stats_from_cpu_memory(linux.cpu.as_ref(), linux.memory.as_ref()),
        containers,
        ephemeral_storage_usage_bytes,
    })
}

fn sandbox_labels(id: &PodId) -> HashMap<String, String> {
    HashMap::from([
        (POD_UID_LABEL.to_string(), id.uid.clone()),
        (POD_NAME_LABEL.to_string(), id.name.clone()),
        (POD_NS_LABEL.to_string(), id.namespace.clone()),
    ])
}

/// `EphemeralContainer` has the same shape as `Container` minus a couple of
/// fields real kubelet itself doesn't honor for debug containers (`ports`,
/// notably — see the API doc comment on `EphemeralContainer.ports`) plus
/// `targetContainerName` (process-namespace-sharing target, not something
/// CRI's `ContainerConfig` has a slot for — nodelet always shares the
/// sandbox's containers via the sandbox's own PID namespace already, so this
/// is a no-op here rather than a gap).
fn ephemeral_to_container(ec: &EphemeralContainer) -> Container {
    Container {
        args: ec.args.clone(),
        command: ec.command.clone(),
        env: ec.env.clone(),
        env_from: ec.env_from.clone(),
        image: ec.image.clone(),
        image_pull_policy: ec.image_pull_policy.clone(),
        lifecycle: ec.lifecycle.clone(),
        liveness_probe: ec.liveness_probe.clone(),
        name: ec.name.clone(),
        ports: None,
        readiness_probe: ec.readiness_probe.clone(),
        resize_policy: ec.resize_policy.clone(),
        resources: ec.resources.clone(),
        restart_policy: ec.restart_policy.clone(),
        security_context: ec.security_context.clone(),
        startup_probe: ec.startup_probe.clone(),
        stdin: ec.stdin,
        stdin_once: ec.stdin_once,
        termination_message_path: ec.termination_message_path.clone(),
        termination_message_policy: ec.termination_message_policy.clone(),
        tty: ec.tty,
        volume_devices: ec.volume_devices.clone(),
        volume_mounts: ec.volume_mounts.clone(),
        working_dir: ec.working_dir.clone(),
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ContainerKind {
    App,
    Init,
    Ephemeral,
}

fn container_labels(id: &PodId, container_name: &str, kind: ContainerKind) -> HashMap<String, String> {
    let mut l = sandbox_labels(id);
    l.insert(CTR_NAME_LABEL.to_string(), container_name.to_string());
    match kind {
        ContainerKind::App => {}
        ContainerKind::Init => {
            l.insert(CTR_INIT_LABEL.to_string(), "true".to_string());
        }
        ContainerKind::Ephemeral => {
            l.insert(CTR_EPHEMERAL_LABEL.to_string(), "true".to_string());
        }
    }
    l
}

/// `userns_mapping`, if `Some((host_id_base, length))`, means this pod's
/// sandbox already has an exclusive UID/GID range allocated for it
/// (`spec.hostUsers: false` — see `userns.rs`, round 25); the caller
/// (`run_sandbox()`) is responsible for the actual allocation since this
/// function stays pure/side-effect-free for testability. `None` (the
/// overwhelmingly common case, `hostUsers` unset or `true`) means no user
/// namespace at all — identical to this function's pre-round-25 behavior.
fn sandbox_config(
    id: &PodId,
    userns_mapping: Option<(u32, u32)>,
    hostname: &str,
    sysctls: &HashMap<String, String>,
) -> PodSandboxConfig {
    // Host-network pods set the network namespace to NODE, which makes the CRI
    // runtime skip CNI entirely (no pod network to set up). The `linux` block
    // is now always built (round 40) — CRI's own proto default for an unset
    // `pid` mode is `POD` (every container shares one PID namespace), the
    // *opposite* of real Kubernetes' actual default (each container gets its
    // own); always setting it explicitly is the fix, not an edge case.
    let userns_options = userns_mapping.map(|(host_id, length)| {
        let mapping = |container_id| IdMapping { host_id, container_id, length };
        UserNamespace { mode: NamespaceMode::Pod as i32, uids: vec![mapping(0)], gids: vec![mapping(0)] }
    });
    // IPC has no CONTAINER-scope concept in the Kubernetes API — containers
    // in a pod always share it unless `hostIPC` opts into sharing the host's.
    let ipc = if id.host_ipc { NamespaceMode::Node } else { NamespaceMode::Pod };
    let network = if id.host_network { NamespaceMode::Node } else { NamespaceMode::Pod };
    let linux = Some(LinuxPodSandboxConfig {
        security_context: Some(LinuxSandboxSecurityContext {
            namespace_options: Some(NamespaceOption {
                network: network as i32,
                pid: pid_namespace_mode(id.host_pid, id.share_process_namespace) as i32,
                ipc: ipc as i32,
                userns_options,
                ..Default::default()
            }),
            ..Default::default()
        }),
        sysctls: sysctls.clone(),
        ..Default::default()
    });

    PodSandboxConfig {
        metadata: Some(PodSandboxMetadata {
            name: id.name.clone(),
            uid: id.uid.clone(),
            namespace: id.namespace.clone(),
            attempt: 0,
        }),
        // Host-network sandboxes share the host UTS namespace, so a hostname
        // cannot be set (runc rejects it). Real kubelets leave it empty too.
        hostname: if id.host_network { String::new() } else { hostname.to_string() },
        log_directory: format!("/var/log/pods/{}_{}_{}", id.namespace, id.name, id.uid),
        labels: sandbox_labels(id),
        linux,
        ..Default::default()
    }
}

/// Event subscriber: prefer the CRI-standard `GetContainerEvents` (works on
/// containerd >= 1.7 and CRI-O); if the runtime doesn't implement it, fall back
/// to containerd's top-level `Events/Subscribe` API (present in every containerd
/// version). Either way, changed pod keys are pushed onto `tx` — no polling.
async fn event_loop(channel: Channel, tx: UnboundedSender<String>) {
    if run_cri_events(&channel, &tx).await == EventOutcome::Unsupported {
        info!("CRI GetContainerEvents unsupported; using containerd native events API");
        containerd_events_loop(channel, tx).await;
    }
}

#[derive(PartialEq)]
enum EventOutcome {
    Unsupported,
    ReceiverGone,
}

/// Returns `Unsupported` if the runtime lacks `GetContainerEvents` (caller should
/// fall back); otherwise keeps reconnecting and only returns when `tx` is closed.
async fn run_cri_events(channel: &Channel, tx: &UnboundedSender<String>) -> EventOutcome {
    loop {
        let mut client = RuntimeServiceClient::new(channel.clone());
        match client.get_container_events(GetEventsRequest::default()).await {
            Ok(resp) => {
                let mut stream = resp.into_inner();
                loop {
                    match stream.message().await {
                        Ok(Some(ev)) => {
                            if let Some(meta) = ev.pod_sandbox_status.and_then(|s| s.metadata) {
                                let key = super::pod_key(&meta.namespace, &meta.name);
                                debug!(pod = %key, "CRI container event");
                                if tx.send(key).is_err() {
                                    return EventOutcome::ReceiverGone;
                                }
                            }
                        }
                        Ok(None) => break, // stream ended; reconnect
                        Err(e) => {
                            warn!(error = ?e, "CRI event stream error");
                            break;
                        }
                    }
                }
            }
            Err(e) if e.code() == tonic::Code::Unimplemented => return EventOutcome::Unsupported,
            Err(e) => warn!(error = ?e, "failed to open CRI event stream"),
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

/// Fallback: subscribe to containerd's native event firehose in the `k8s.io`
/// namespace, watch `/tasks/*` events, map the container id back to a pod via its
/// labels, and push the pod key. Reconnects on error.
async fn containerd_events_loop(channel: Channel, tx: UnboundedSender<String>) {
    loop {
        let mut client = EventsClient::new(channel.clone());
        // Empty filters = whole firehose; we scope to k8s.io via the namespace
        // header and filter topics client-side (robust against filter grammar).
        let mut req = tonic::Request::new(SubscribeRequest { filters: vec![] });
        req.metadata_mut()
            .insert("containerd-namespace", "k8s.io".parse().unwrap());

        match client.subscribe(req).await {
            Ok(resp) => {
                let mut stream = resp.into_inner();
                loop {
                    match stream.message().await {
                        Ok(Some(env)) => {
                            if !env.topic.starts_with("/tasks/") {
                                continue;
                            }
                            let Some(cid) = env
                                .event
                                .and_then(|a| TaskEventContainerId::decode(a.value.as_slice()).ok())
                                .map(|t| t.container_id)
                                .filter(|c| !c.is_empty())
                            else {
                                continue;
                            };
                            debug!(topic = %env.topic, container = %cid, "containerd task event");
                            if let Some((ns, name)) = lookup_pod_by_cid(channel.clone(), &cid).await {
                                if tx.send(super::pod_key(&ns, &name)).is_err() {
                                    return; // controller dropped the receiver
                                }
                            }
                        }
                        Ok(None) => break,
                        Err(e) => {
                            warn!(error = ?e, "containerd event stream error");
                            break;
                        }
                    }
                }
            }
            Err(e) => warn!(error = ?e, "failed to subscribe to containerd events"),
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

// Small, isolated test files — one behavior area each — under cri_tests/.
// `#[path]` keeps them in their own files while still being submodules of
// this one, so they can see its private items (compute_phase,
// restart_decision, build_mounts, write_volume_dir, pod_id, the label/
// sandbox_config builders) without anything needing to be made `pub`.
#[cfg(test)]
#[path = "cri_tests/sandbox_reuse.rs"]
mod tests_sandbox_reuse;
#[cfg(test)]
#[path = "cri_tests/phase.rs"]
mod tests_phase;
#[cfg(test)]
#[path = "cri_tests/restart_decision.rs"]
mod tests_restart_decision;
#[cfg(test)]
#[path = "cri_tests/sidecar_init_decision.rs"]
mod tests_sidecar_init_decision;
#[cfg(test)]
#[path = "cri_tests/mounts.rs"]
mod tests_mounts;
#[cfg(test)]
#[path = "cri_tests/volume_type.rs"]
mod tests_volume_type;
#[cfg(test)]
#[path = "cri_tests/service_env.rs"]
mod tests_service_env;
#[cfg(test)]
#[path = "cri_tests/write_volume_dir.rs"]
mod tests_write_volume_dir;
#[cfg(test)]
#[path = "cri_tests/pod_id.rs"]
mod tests_pod_id;
#[cfg(test)]
#[path = "cri_tests/labels.rs"]
mod tests_labels;
#[cfg(test)]
#[path = "cri_tests/sandbox_config.rs"]
mod tests_sandbox_config;
#[cfg(test)]
#[path = "cri_tests/linux_resources.rs"]
mod tests_linux_resources;
#[cfg(test)]
#[path = "cri_tests/linux_security_context.rs"]
mod tests_linux_security_context;
#[cfg(test)]
#[path = "cri_tests/dns_config.rs"]
mod tests_dns_config;
#[cfg(test)]
#[path = "cri_tests/registry_auth.rs"]
mod tests_registry_auth;
#[cfg(test)]
#[path = "cri_tests/init_container_decision.rs"]
mod tests_init_container_decision;
#[cfg(test)]
#[path = "cri_tests/termination_grace.rs"]
mod tests_termination_grace;
#[cfg(test)]
#[path = "cri_tests/restart_count.rs"]
mod tests_restart_count;
#[cfg(test)]
#[path = "cri_tests/downward_api_volume.rs"]
mod tests_downward_api_volume;
#[cfg(test)]
#[path = "cri_tests/projected_keys.rs"]
mod tests_projected_keys;
#[cfg(test)]
#[path = "cri_tests/fs_group.rs"]
mod tests_fs_group;
#[cfg(test)]
#[path = "cri_tests/etc_hosts.rs"]
mod tests_etc_hosts;
#[cfg(test)]
#[path = "cri_tests/log_rotation.rs"]
mod tests_log_rotation;
#[cfg(test)]
#[path = "cri_tests/service_account_token.rs"]
mod tests_service_account_token;
#[cfg(test)]
#[path = "cri_tests/resource_list_to_linux_resources.rs"]
mod tests_resource_list_to_linux_resources;
#[cfg(test)]
#[path = "cri_tests/extended_resource_requests.rs"]
mod tests_extended_resource_requests;
#[cfg(test)]
#[path = "cri_tests/csi_attach.rs"]
mod tests_csi_attach;
#[cfg(test)]
#[path = "cri_tests/termination_message.rs"]
mod tests_termination_message;
#[cfg(test)]
#[path = "cri_tests/tmpfs_empty_dir.rs"]
mod tests_tmpfs_empty_dir;
#[cfg(test)]
#[path = "cri_tests/ephemeral_volume.rs"]
mod tests_ephemeral_volume;
#[cfg(test)]
#[path = "cri_tests/pod_hostname.rs"]
mod tests_pod_hostname;
#[cfg(test)]
#[path = "cri_tests/pid_namespace_mode.rs"]
mod tests_pid_namespace_mode;
#[cfg(test)]
#[path = "cri_tests/pod_sysctls.rs"]
mod tests_pod_sysctls;
#[cfg(test)]
#[path = "cri_tests/resize_decision.rs"]
mod tests_resize_decision;
#[cfg(test)]
#[path = "cri_tests/resource_field_ref.rs"]
mod tests_resource_field_ref;
#[cfg(test)]
#[path = "cri_tests/csi_ephemeral_volume_handle.rs"]
mod tests_csi_ephemeral_volume_handle;
#[cfg(test)]
#[path = "cri_tests/directory_usage_bytes.rs"]
mod tests_directory_usage_bytes;
#[cfg(test)]
#[path = "cri_tests/image_pull_policy.rs"]
mod tests_image_pull_policy;
#[cfg(test)]
#[path = "cri_tests/runtime_handler_from_cri.rs"]
mod tests_runtime_handler_from_cri;
#[cfg(test)]
#[path = "cri_tests/format_container_id.rs"]
mod tests_format_container_id;
#[cfg(test)]
#[path = "cri_tests/hugepage_cri_page_size.rs"]
mod tests_hugepage_cri_page_size;

/// Map a containerd container/sandbox id back to its pod (namespace, name) via
/// the `nodelet.dev/*` labels we stamped on it.
async fn lookup_pod_by_cid(channel: Channel, cid: &str) -> Option<(String, String)> {
    fn ns_name(labels: &HashMap<String, String>) -> Option<(String, String)> {
        Some((labels.get(POD_NS_LABEL)?.clone(), labels.get(POD_NAME_LABEL)?.clone()))
    }

    // App containers first.
    let mut rt = RuntimeServiceClient::new(channel.clone());
    if let Ok(resp) = rt
        .list_containers(ListContainersRequest {
            filter: Some(ContainerFilter { id: cid.to_string(), ..Default::default() }),
        })
        .await
    {
        if let Some(c) = resp.into_inner().containers.into_iter().next() {
            if let Some(p) = ns_name(&c.labels) {
                return Some(p);
            }
        }
    }

    // Otherwise the id may be a pod sandbox (e.g. the pause container's task).
    let mut rt = RuntimeServiceClient::new(channel);
    if let Ok(resp) = rt
        .list_pod_sandbox(ListPodSandboxRequest {
            filter: Some(PodSandboxFilter { id: cid.to_string(), ..Default::default() }),
        })
        .await
    {
        if let Some(s) = resp.into_inner().items.into_iter().next() {
            return ns_name(&s.labels);
        }
    }
    None
}
