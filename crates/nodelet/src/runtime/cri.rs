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
    SecretReference, SecurityContext, Service, ServiceAccount, Volume,
};
use k8s_openapi::api::node::v1::RuntimeClass;
// resource.k8s.io/v1beta1 no longer exists on modern (1.34+) clusters —
// DRA graduated to GA as resource.k8s.io/v1. k8s-openapi's *generated*
// ResourceClaim type exists now too (workspace bumped to "v1_34"), but this
// raw-request copy hasn't been retired yet — separate follow-up work, not a
// rider on the version bump. So ResourceClaim is still fetched via a raw
// request into `claims::RawResourceClaim` (hand-written, only the fields
// DRA actually reads) instead of a typed `kube::Api<T>` — see that module's
// own doc comment. Using the removed v1beta1 type here previously made
// every ResourceClaim fetch 404 silently (claims.rs logs and skips on
// fetch failure rather than failing the pod), so containers
// requesting a device via DRA/CDI always started with none — found live
// standing up a real DRA driver for e2e testing (round 121).
use k8s_openapi::api::storage::v1::{CSIDriver, VolumeAttachment};
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use k8s_openapi::apimachinery::pkg::util::intstr::IntOrString;
use k8s_openapi::jiff::Timestamp;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use kube::api::{Api, ListParams};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};
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
    IdMapping, UserNamespace, PortMapping, Protocol, MountPropagation,
};

/// Where ConfigMap/Secret volume contents get materialized on the host, one
/// subdirectory per pod UID — mirrors a real kubelet's
/// /var/lib/kubelet/pods/<uid>/volumes/ layout closely enough that this is
/// recognizable, without trying to be a drop-in match.

mod claims;
mod container_create;
mod container_state;
mod container_support;
mod env;
mod events_gc;
mod pod_resources_snapshot;
mod pod_runtime_impl;
mod resources;
mod sandbox;
mod status;
mod volumes_pure;
mod volumes_resolve;

pub(crate) use claims::*;
// These 3 submodules are entirely `impl CriRuntime { ... }` methods (called
// as `self.method()`, which resolves regardless of import status) or a
// single trait impl (active once compiled, no import needed either) — no
// free function in them is ever referenced by bare name from a sibling
// submodule, so the re-export itself is genuinely unused, not a mistake.
#[allow(unused_imports)]
pub(crate) use container_create::*;
pub(crate) use container_state::*;
pub(crate) use container_support::*;
pub(crate) use env::*;
pub(crate) use events_gc::*;
#[allow(unused_imports)]
pub(crate) use pod_runtime_impl::*;
pub(crate) use resources::*;
pub(crate) use sandbox::*;
pub(crate) use status::*;
pub(crate) use volumes_pure::*;
#[allow(unused_imports)]
pub(crate) use volumes_resolve::*;

const VOLUME_ROOT: &str = "/var/lib/nodelet/pods";

/// Where device-plugin allocations are checkpointed to disk (round 124) —
/// see `device_alloc_checkpoint_path()`'s own doc comment (container_
/// support.rs) for why this needs to survive a nodelet restart the same
/// way a CSI volume's `MountMeta` sidecar does.
const DEVICE_ALLOC_CHECKPOINT_DIR: &str = "/var/lib/nodelet/device-plugins/allocations";

/// `terminationMessagePath`'s effective cap (round 24) — real kubelet's own
/// `kubecontainer.MaxContainerTerminationMessageLength`. A container that
/// writes more than this to its termination-log file only has the last
/// this-many bytes read back (matching upstream: the most recent content
/// is what usually matters for a short human-readable failure reason, not
/// whatever was written first).
const MAX_TERMINATION_MESSAGE_BYTES: usize = 4096;

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
    /// Shared informer-style cache of Services used to inject service-link
    /// environment variables. Listing every Service on every pod reconcile
    /// made a status/watch event pay an apiserver round trip even when the
    /// container was already running; under nodecontroller's full watch and
    /// controller workload that became a visible pod-start latency tax. The
    /// cache is kept current by one watch, rather than a TTL.
    service_cache: Arc<RwLock<env::ServiceCache>>,
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
    /// `"namespace/name" -> a per-pod async lock`, held for the whole
    /// duration of `ensure_pod()` (round 123; found live in CI). `ensure_pod()`
    /// is called from at least four independent, unsynchronized places for
    /// the same logical pod — the main watch-event reconcile loop, its own
    /// `schedule_retry()` (a detached `tokio::spawn`ed task), and
    /// `probes.rs`'s `restart_and_reensure()` (called directly on `Arc<dyn
    /// PodRuntime>`, bypassing `PodController` entirely) — and a
    /// probe-triggered `restart_container()` call itself generates a real
    /// CRI container-died event that can independently re-trigger the main
    /// reconcile loop's own `ensure_pod()` concurrently with
    /// `restart_and_reensure()`'s direct one. Without serialization, two
    /// Concurrent pod lifecycle operations for the same name both compute the
    /// identical `attempt` number (`restart_count()`, unchanged since
    /// neither has succeeded yet) and race on `CreateContainer`, which
    /// containerd correctly rejects with "failed to reserve container
    /// name ... is reserved for <the other one>" — confirmed live: a
    /// liveness-triggered restart repeatedly hit exactly this, never
    /// actually recreating the container within the test's own 60s
    /// timeout. The same lock also serializes detached teardown with a new
    /// `ensure_pod()` for a deleted-and-recreated namespace/name; without
    /// that, old teardown can remove the replacement's sandbox or leave
    /// UID-bound projected-token state referring to the old Pod. Keyed by
    /// "namespace/name" (not sandbox id, which doesn't exist yet on the
    /// very first `ensure_pod()` call) — matches the key shape every
    /// `warn!` log line in this area already uses. Entries are never removed
    /// (a small, bounded amount of memory per pod that's ever existed on this
    /// node — same simplification `restart_counts`/`restart_backoff` already
    /// make, cleaned up implicitly by node reboot, not worth the bookkeeping
    /// to prune per-pod-deletion for a single empty `Mutex<()>` each).
    pod_ensure_locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
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
    /// Node swap capacity in bytes (round 68) — the `TotalPodsSwapAvailable`
    /// input to `container_swap_limit_bytes()`'s `LimitedSwap` formula.
    /// Read once at startup from `/proc/meminfo`'s `SwapTotal` (total
    /// capacity, not currently-free swap — a container's computed swap
    /// share must stay stable across reconciles regardless of what else on
    /// the node happens to be using swap right now). `0` on a swapless
    /// node, which the formula already treats as "no swap to allocate."
    node_swap_bytes: i64,
    /// `NODELET_MEMORY_SWAP_BEHAVIOR` (round 68; GA 1.34, found in round
    /// 65's fresh gap re-audit) — `true` means `LimitedSwap` (Burstable-QoS
    /// containers get a proportional swap allowance), `false` (the
    /// default, matching upstream) means `NoSwap` (every container's swap
    /// is explicitly capped at its own memory limit, i.e. zero additional
    /// swap — not just left unconfigured, which on a node with swap
    /// already enabled at the OS level wouldn't actually prevent swap use).
    memory_swap_limited: bool,
    /// Filesystem path image-GC disk usage is measured against (round 70)
    /// — same `NODELET_DISK_PATH` `DiskPressure` already reads.
    disk_path: String,
    /// `NODELET_IMAGE_GC_HIGH_THRESHOLD_PERCENT`/`_LOW_THRESHOLD_PERCENT`/
    /// `_MIN_AGE_SECS` (round 70; found in round 69's fresh gap re-audit)
    /// — real kubelet's image-GC watermark policy, replacing the
    /// pre-round-70 behavior of sweeping every unreferenced image on
    /// every GC cycle regardless of actual disk pressure. See
    /// `gc.rs::should_start_image_gc()`/`images_to_reclaim_space()`.
    image_gc_high_threshold_percent: u8,
    image_gc_low_threshold_percent: u8,
    image_gc_min_age_secs: u64,
    /// `image id -> unix seconds first observed unreferenced` (round 70)
    /// — real kubelet's own `--image-minimum-gc-age` reference point.
    /// Rebuilt incrementally every GC cycle in `gc_unreferenced_images()`:
    /// an id not currently unreferenced is dropped (its clock resets if
    /// it becomes unreferenced again later, an acceptable simplification
    /// vs. upstream's own richer access-time tracking); a newly-unreferenced
    /// id is inserted with the current time.
    image_unreferenced_since: Mutex<HashMap<String, u64>>,
    /// `NODELET_IMAGE_CREDENTIAL_PROVIDER_CONFIG`/`_BIN_DIR` (round 71) —
    /// `None` when unconfigured (the default), meaning `resolve_pull_auth()`
    /// only ever tries `imagePullSecrets`, exactly the pre-round-71
    /// behavior.
    credential_providers: Option<crate::credential_provider::CredentialProviders>,
    /// This CRI runtime's own name (e.g. `"containerd"`), from a one-time
    /// `Version` RPC call made in `connect()` (round 57; found in round
    /// 54's re-audit) — real kubelet always formats `ContainerStatus.containerID`/
    /// `state.terminated.containerID` as `<runtimeName>://<id>`, built
    /// from this exact same call. `"unknown"` if the call ever fails
    /// (best-effort — a cosmetic prefix isn't worth failing `connect()`
    /// over).
    runtime_name: String,
    /// `handler name -> whether it advertises `recursiveReadOnlyMounts`
    /// support` (round 97; closes round 85's documented `IfPossible`
    /// simplification), from the same one-time `Status` RPC call
    /// `runtime_handlers()` makes on demand for `Node.status.runtimeHandlers`
    /// (round 53) — cached here instead so `volumeMounts[].recursiveReadOnly:
    /// IfPossible` can make a real per-mount decision without an extra RPC
    /// on every container creation. The empty-string key is CRI's own
    /// convention for "the default handler" (no `runtimeClassName` set).
    /// Best-effort: empty (every handler treated as unsupported, same as
    /// this codebase's pre-round-97 behavior for `IfPossible`) if the
    /// `Status` call fails at connect time.
    recursive_read_only_handlers: HashMap<String, bool>,
    /// `"sandbox_id/container_name" -> cumulative restart count`, mirroring
    /// `PodStatus.containerStatuses[].restartCount`. Side table for the same
    /// reason `restart_policies` is one: CRI's `ListContainers` has no
    /// concept of "how many times has this logical container restarted" —
    /// each restart is a brand-new container id, so nodelet has to count.
    restart_counts: Mutex<HashMap<String, u32>>,
    /// `"sandbox_id/container_name" -> (unix seconds of the last restart
    /// attempt, the backoff delay in seconds that was required before
    /// that attempt)` — crash-loop backoff state (round 73; found in
    /// round 72's re-audit). Without this, a container stuck exiting
    /// immediately after every start gets recreated as fast as the
    /// runtime allows, since this codebase's own status-write-triggers-
    /// another-watch-event feedback loop re-drives `reconcile()` far
    /// faster than any human-perceptible interval. See
    /// `container_state.rs`'s `crash_loop_backoff_secs()`/
    /// `crash_loop_backoff_ready()`.
    restart_backoff: Mutex<HashMap<String, (u64, u64)>>,
    /// `"sandbox_id/container_name" -> (unix seconds of the last failed
    /// pull attempt, the backoff delay in seconds required before the
    /// next one, cumulative failed-attempt count)` — round 124 (found
    /// live in CI): a separate table from `restart_backoff` above so an
    /// image-pull backoff (before the container has ever existed at all)
    /// and a post-start crash-loop backoff never collide on the same
    /// key. The attempt count (not just presence/absence) is what lets
    /// `pull_backoff_reason()` distinguish the first failure
    /// (`ErrImagePull`, matching real kubelet) from every one after
    /// (`ImagePullBackOff`). See `container_state.rs`'s
    /// `pull_backoff_ready()`/`record_pull_backoff()`.
    pull_backoff: Mutex<HashMap<String, (u64, u64, u32)>>,
    /// `"sandbox_id/container_name" -> the previous container instance's
    /// terminated details`, captured right before that instance is
    /// removed to make way for a fresh one (round 75; found in round
    /// 73's crash-loop backoff work) — feeds
    /// `containerStatuses[].lastState`, which otherwise has no way to
    /// stay populated once the exited instance it describes is gone and
    /// a new (possibly still-running) one has replaced it.
    last_terminated: Mutex<HashMap<String, crate::runtime::TerminatedInfo>>,
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
    /// Dynamic Resource Allocation driver registry (round 63; see `dra.rs`)
    /// — populated entirely via dynamic registration, same reason
    /// `device_plugins` has no static-config equivalent: a DRA driver's
    /// endpoint is meaningless without the driver process itself running.
    dra: Arc<crate::dra::DraDrivers>,
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
    /// `"sandbox_id/container_name" -> (uid, gid, supplemental_groups)`
    /// (round 90; found in round 89's re-audit) — `ContainerStatus.user`,
    /// fetched exactly *once* right after a container starts (not on
    /// every reconcile — `build_status()`'s "zero extra RPCs for a
    /// healthy container" design, round 24, still holds) and cached here
    /// for the rest of that container instance's life. A fresh restart
    /// re-fetches, since a new instance could resolve to a different
    /// user (e.g. if the image itself changed).
    container_users: Mutex<HashMap<String, (i64, i64, Vec<i64>)>>,
    /// `"sandbox_id/container_name" -> [(volume name, mount path, read_only,
    /// recursive_read_only)]` (round 91; found in round 89's re-audit) —
    /// `containerStatuses[].volumeMounts`. Unlike `container_users` above,
    /// this needs no RPC at all: it's entirely derived from the container's
    /// own `volumeMounts` spec, which is already in hand right where
    /// `build_mounts()` is called at container-creation time — computed and
    /// cached there once, read back by `build_status()` via plain lookup.
    container_volume_mount_statuses: Mutex<HashMap<String, Vec<(String, String, bool, Option<String>)>>>,
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
        node_swap_bytes: i64,
        memory_swap_limited: bool,
        disk_path: String,
        image_gc_high_threshold_percent: u8,
        image_gc_low_threshold_percent: u8,
        image_gc_min_age_secs: u64,
        image_credential_provider_config: String,
        image_credential_provider_bin_dir: String,
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

        // Which handlers advertise recursiveReadOnlyMounts support (round
        // 97), from the same Status RPC `runtime_handlers()` makes on
        // demand for Node.status.runtimeHandlers — cached once here so
        // volumeMounts[].recursiveReadOnly: IfPossible can make a real
        // per-mount decision without an extra RPC on every container
        // creation. Best-effort: empty (every handler treated as
        // unsupported) if this call fails.
        let mut status_client = rt.clone();
        let recursive_read_only_handlers = match status_client.status(StatusRequest { verbose: false }).await {
            Ok(resp) => resp
                .into_inner()
                .runtime_handlers
                .into_iter()
                .map(|h| (h.name, h.features.map(|f| f.recursive_read_only_mounts).unwrap_or(false)))
                .collect(),
            Err(e) => {
                warn!(error = ?e, "CRI Status call failed; volumeMounts[].recursiveReadOnly: IfPossible will fall back to non-recursive for every handler");
                HashMap::new()
            }
        };

        // Spawn the event subscriber (event-driven status, no polling).
        let (tx, rx) = unbounded_channel();
        tokio::spawn(event_loop(channel, tx.clone()));

        // Best-effort: a malformed/unreadable CredentialProviderConfig
        // shouldn't block startup any more than a missing one does —
        // logged loudly (real misconfiguration, worth noticing) and
        // falls back to "feature off" rather than failing connect().
        let credential_providers = match crate::credential_provider::CredentialProviders::load(
            &image_credential_provider_config,
            &image_credential_provider_bin_dir,
        ) {
            Ok(cp) => cp,
            Err(e) => {
                warn!(error = ?e, "failed to load CredentialProviderConfig; image credential providers disabled for this run");
                None
            }
        };

        let csi = Arc::new(crate::runtime::csi::CsiDrivers::new(csi_drivers));
        // Round 124: shares the same event channel event_loop above feeds
        // — a device health transition is exactly the same shape of
        // "real state change that never touches the Pod object" as a
        // container exit event, so it re-triggers pods.rs's
        // on_runtime_event() the same way. See DevicePlugins::owners'
        // own doc comment for the full story.
        let device_plugins = Arc::new(crate::device_plugins::DevicePlugins::new(tx.clone()));
        let dra = Arc::new(crate::dra::DraDrivers::new());
        let service_cache = Arc::new(RwLock::new(env::ServiceCache::default()));
        tokio::spawn(env::service_cache_loop(client.clone(), service_cache.clone()));
        // Dynamic CSI driver / device plugin / DRA driver discovery: watches
        // plugin_registry_path for a plugin's registrar socket, same
        // protocol real kubelet's own plugin watcher speaks (see
        // plugin_registry.rs) — a no-op loop if nothing ever registers there.
        tokio::spawn(crate::plugin_registry::run(
            csi.clone(),
            device_plugins.clone(),
            dra.clone(),
            plugin_registry_path,
            plugin_registry_sync_interval,
            client.clone(),
            node_name.clone(),
        ));

        Ok(Self {
            rt,
            img,
            client,
            service_cache,
            node_name,
            cluster_dns,
            cluster_domain,
            rx: Mutex::new(Some(rx)),
            restart_policies: Mutex::new(HashMap::new()),
            pod_ensure_locks: Mutex::new(HashMap::new()),
            pod_uids: Mutex::new(HashMap::new()),
            sidecar_names: Mutex::new(HashMap::new()),
            userns,
            node_memory_bytes,
            node_cpu_millicores,
            node_swap_bytes,
            memory_swap_limited,
            disk_path,
            image_gc_high_threshold_percent,
            image_gc_low_threshold_percent,
            image_gc_min_age_secs,
            image_unreferenced_since: Mutex::new(HashMap::new()),
            credential_providers,
            runtime_name,
            recursive_read_only_handlers,
            restart_counts: Mutex::new(HashMap::new()),
            restart_backoff: Mutex::new(HashMap::new()),
            pull_backoff: Mutex::new(HashMap::new()),
            last_terminated: Mutex::new(HashMap::new()),
            csi,
            device_plugins,
            device_allocations: Mutex::new(HashMap::new()),
            dra,
            cpu_manager,
            memory_manager,
            container_resources: Mutex::new(HashMap::new()),
            container_users: Mutex::new(HashMap::new()),
            container_volume_mount_statuses: Mutex::new(HashMap::new()),
            applied_resources: Mutex::new(HashMap::new()),
            spec_resources: Mutex::new(HashMap::new()),
            topology_policy,
            numa_topology,
        })
    }

    /// Whether `handler` (a resolved `RuntimeClass` handler name, or the
    /// empty string for the default handler — same convention
    /// `resolve_runtime_handler()` returns) advertises
    /// `recursiveReadOnlyMounts` support, per the one-time `Status` RPC
    /// result cached at `connect()` time (round 97).
    pub(crate) fn handler_supports_recursive_ro(&self, handler: &str) -> bool {
        self.recursive_read_only_handlers.get(handler).copied().unwrap_or(false)
    }

    /// The per-pod async lifecycle lock held for the whole duration of
    /// `ensure_pod()` or `remove_pod()` — see `pod_ensure_locks`'s own doc
    /// comment for why. `key` is `"namespace/name"`.
    pub(crate) fn pod_ensure_lock(&self, key: &str) -> Arc<tokio::sync::Mutex<()>> {
        self.pod_ensure_locks.lock().unwrap().entry(key.to_string()).or_insert_with(|| Arc::new(tokio::sync::Mutex::new(()))).clone()
    }
}

// Small, isolated test files — one behavior area each — under cri_tests/.
// `#[path]` keeps them in their own files while still being submodules of
// this one, so they can see its private items (compute_phase,
// restart_decision, build_mounts, write_volume_dir, pod_id, the label/
// sandbox_config builders) without anything needing to be made `pub` — via
// this file's own blanket `pub(crate) use submodule::*;` re-exports.
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
#[path = "cri_tests/command_args_expansion.rs"]
mod tests_command_args_expansion;
#[cfg(test)]
#[path = "cri_tests/mount_propagation.rs"]
mod tests_mount_propagation;
#[cfg(test)]
#[path = "cri_tests/recursive_read_only.rs"]
mod tests_recursive_read_only;
#[cfg(test)]
#[path = "cri_tests/devices.rs"]
mod tests_devices;
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
#[path = "cri_tests/port_mappings.rs"]
mod tests_port_mappings;
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
#[path = "cri_tests/proc_mount_paths.rs"]
mod tests_proc_mount_paths;
#[cfg(test)]
#[path = "cri_tests/resource_health_string.rs"]
mod tests_resource_health_string;
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
#[path = "cri_tests/crash_loop_backoff.rs"]
mod tests_crash_loop_backoff;
#[cfg(test)]
#[path = "cri_tests/last_terminated.rs"]
mod tests_last_terminated;
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
#[path = "cri_tests/hugetlbfs_empty_dir.rs"]
mod tests_hugetlbfs_empty_dir;
#[cfg(test)]
#[path = "cri_tests/dra_claim_devices.rs"]
mod tests_dra_claim_devices;
#[cfg(test)]
#[path = "cri_tests/host_path.rs"]
mod tests_host_path;
#[cfg(test)]
#[path = "cri_tests/stop_signal.rs"]
mod tests_stop_signal;
#[cfg(test)]
#[path = "cri_tests/swap_limit.rs"]
mod tests_swap_limit;
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
#[cfg(test)]
#[path = "cri_tests/volume_mount_status_tuples.rs"]
mod tests_volume_mount_status_tuples;
#[cfg(test)]
#[path = "cri_tests/pending_csi_volume_names.rs"]
mod tests_pending_csi_volume_names;
