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
use anyhow::{Context, Result};
use async_trait::async_trait;
use k8s_openapi::api::core::v1::{
    ConfigMap, Container, EnvFromSource, EnvVarSource, EphemeralContainer, LifecycleHandler,
    PersistentVolume, PersistentVolumeClaim, Pod, PodSecurityContext, ResourceRequirements, Secret,
    SecretReference, SecurityContext, Service, Volume,
};
use k8s_openapi::api::node::v1::RuntimeClass;
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use k8s_openapi::apimachinery::pkg::util::intstr::IntOrString;
use k8s_openapi::jiff::Timestamp;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use kube::api::{Api, ListParams};
use std::collections::{BTreeMap, HashMap, HashSet};
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
    ContainerStatusRequest, CreateContainerRequest, DnsConfig, GetEventsRequest, ImageSpec, Int64Value,
    KeyValue, LinuxContainerConfig, LinuxContainerResources, LinuxContainerSecurityContext,
    LinuxPodSandboxConfig, LinuxSandboxSecurityContext, ListContainersRequest, ListImagesRequest,
    ListPodSandboxRequest, NamespaceMode, NamespaceOption, PodSandboxConfig, PodSandboxFilter,
    PodSandboxMetadata, PodSandboxStatusRequest, PullImageRequest, Mount, RemoveContainerRequest,
    RemoveImageRequest, RemovePodSandboxRequest, RunPodSandboxRequest, StartContainerRequest,
    StopContainerRequest, StopPodSandboxRequest, ExecSyncRequest, ReopenContainerLogRequest,
    ExecRequest, AttachRequest, PortForwardRequest, ListPodSandboxStatsRequest,
    security_profile::ProfileType, SecurityProfile,
};

/// Where ConfigMap/Secret volume contents get materialized on the host, one
/// subdirectory per pod UID — mirrors a real kubelet's
/// /var/lib/kubelet/pods/<uid>/volumes/ layout closely enough that this is
/// recognizable, without trying to be a drop-in match.
const VOLUME_ROOT: &str = "/var/lib/nodelet/pods";

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
}

/// Identity extracted from a Pod object.
struct PodId {
    namespace: String,
    name: String,
    uid: String,
    host_network: bool,
}

fn pod_id(pod: &Pod) -> PodId {
    let namespace = pod.metadata.namespace.clone().unwrap_or_else(|| "default".to_string());
    let name = pod.metadata.name.clone().unwrap_or_default();
    let uid = pod
        .metadata
        .uid
        .clone()
        .unwrap_or_else(|| format!("{namespace}_{name}"));
    let host_network = pod.spec.as_ref().and_then(|s| s.host_network).unwrap_or(false);
    PodId { namespace, name, uid, host_network }
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

/// Build CRI `Mount` entries for a container's volumeMounts against the
/// pod's already-resolved volume name -> host directory map (see
/// resolve_volumes()). A mount naming a volume that isn't in the map
/// (unsupported volume type, or the ConfigMap/Secret fetch failed) is
/// silently dropped — pulled out as a pure function specifically to make
/// that behavior, and subPath/readOnly handling, unit-testable without a
/// real CRI socket.
fn build_mounts(
    volume_mounts: &[k8s_openapi::api::core::v1::VolumeMount],
    volumes: &HashMap<String, PathBuf>,
) -> Vec<Mount> {
    volume_mounts
        .iter()
        .filter_map(|vm| {
            let host_dir = volumes.get(&vm.name)?;
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

/// kubelet's cpu.shares formula: `max(2, milliCPU * 1024 / 1000)`. No
/// request/limit at all still gets the cgroup-default minimum (2), same as
/// a real BestEffort pod.
fn cpu_shares_for(cpu_millicores: Option<i64>) -> i64 {
    match cpu_millicores {
        Some(m) if m > 0 => ((m * 1024) / 1000).max(2),
        _ => 2,
    }
}

/// Translate a container's `resources` into CRI's `LinuxContainerResources`.
/// CPU shares come from requests (falling back to limits if there's no
/// request, matching kubelet); CPU quota/period and the memory limit come
/// from limits only — a limit-less resource is left at CRI's "unspecified"
/// zero value, which containerd/runc treat as unconstrained.
fn linux_resources(resources: Option<&ResourceRequirements>) -> LinuxContainerResources {
    let requests = resources.and_then(|r| r.requests.as_ref());
    let limits = resources.and_then(|r| r.limits.as_ref());
    let cpu_request = requests.and_then(|m| m.get("cpu")).and_then(parse_cpu_millicores);
    let cpu_limit = limits.and_then(|m| m.get("cpu")).and_then(parse_cpu_millicores);
    let mem_limit = limits.and_then(|m| m.get("memory")).and_then(parse_memory_bytes);

    let (cpu_quota, cpu_period) = match cpu_limit {
        Some(m) if m > 0 => (CPU_CFS_QUOTA_PERIOD_US * m / 1000, CPU_CFS_QUOTA_PERIOD_US),
        _ => (0, 0),
    };

    LinuxContainerResources {
        cpu_shares: cpu_shares_for(cpu_request.or(cpu_limit)),
        cpu_quota,
        cpu_period,
        memory_limit_in_bytes: mem_limit.unwrap_or(0),
        ..Default::default()
    }
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
        cluster_dns: Vec<String>,
        cluster_domain: String,
        csi_drivers: BTreeMap<String, String>,
        plugin_registry_path: String,
        plugin_registry_sync_interval: Duration,
    ) -> Result<Self> {
        let channel = connect_uds(endpoint).await?;
        let rt = RuntimeServiceClient::new(channel.clone());
        let img = ImageServiceClient::new(channel.clone());

        // Spawn the event subscriber (event-driven status, no polling).
        let (tx, rx) = unbounded_channel();
        tokio::spawn(event_loop(channel, tx));

        let csi = Arc::new(crate::runtime::csi::CsiDrivers::new(csi_drivers));
        // Dynamic CSI driver discovery: watches plugin_registry_path for a
        // driver's registrar socket, same protocol real kubelet's own
        // plugin watcher speaks (see plugin_registry.rs) — a no-op loop
        // wrapper around whatever's already in `csi` if nothing ever
        // registers there.
        tokio::spawn(crate::plugin_registry::run(csi.clone(), plugin_registry_path, plugin_registry_sync_interval));

        Ok(Self {
            rt,
            img,
            client,
            cluster_dns,
            cluster_domain,
            rx: Mutex::new(Some(rx)),
            restart_policies: Mutex::new(HashMap::new()),
            restart_counts: Mutex::new(HashMap::new()),
            csi,
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
    async fn resolve_volumes(&self, pod: &Pod, id: &PodId) -> HashMap<String, PathBuf> {
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
                        out.insert(v.name.clone(), vol_dir);
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
                        out.insert(v.name.clone(), vol_dir);
                    }
                    Err(_) if optional => {}
                    Err(e) => warn!(volume = %v.name, secret = %name, error = ?e, "failed to fetch Secret for volume"),
                }
            } else if v.empty_dir.is_some() {
                if let Err(e) = std::fs::create_dir_all(&vol_dir) {
                    warn!(volume = %v.name, error = ?e, "failed to create emptyDir volume");
                    continue;
                }
                out.insert(v.name.clone(), vol_dir);
            } else if let Some(downward) = &v.downward_api {
                if let Err(e) = write_downward_api_volume(&vol_dir, pod, downward.items.as_deref().unwrap_or(&[])) {
                    warn!(volume = %v.name, error = ?e, "failed to materialize downwardAPI volume");
                    continue;
                }
                out.insert(v.name.clone(), vol_dir);
            } else if let Some(projected) = &v.projected {
                if let Err(e) = self.write_projected_volume(&vol_dir, pod, id, projected).await {
                    warn!(volume = %v.name, error = ?e, "failed to materialize projected volume");
                    continue;
                }
                out.insert(v.name.clone(), vol_dir);
            } else if let Some(pvc_source) = &v.persistent_volume_claim {
                match self.resolve_csi_source(&id.namespace, &pvc_source.claim_name).await {
                    Ok(Some(mut source)) => {
                        source.read_only |= pvc_source.read_only.unwrap_or(false);
                        match self.csi.mount(&source, &vol_dir, &id.uid).await {
                            Ok(()) => {
                                out.insert(v.name.clone(), vol_dir);
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
                    out.insert(ETC_HOSTS_VOLUME_KEY.to_string(), hosts_path);
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
            for (key, dir) in &out {
                if key == ETC_HOSTS_VOLUME_KEY {
                    continue; // a single file, not a directory nodelet materialized as a tree
                }
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
        }))
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
            let Some(pvc_source) = &v.persistent_volume_claim else { continue };
            let source = match self.resolve_csi_source(&id.namespace, &pvc_source.claim_name).await {
                Ok(Some(source)) => source,
                Ok(None) => continue, // already logged why in resolve_csi_source()
                Err(e) => {
                    warn!(volume = %v.name, claim = %pvc_source.claim_name, error = ?e, "CSI teardown: failed to resolve PersistentVolumeClaim; volume left mounted");
                    continue;
                }
            };
            let vol_dir = pod_dir.join(&v.name);
            if let Err(e) = self.csi.unmount(&source.driver, &source.volume_handle, &vol_dir, &id.uid).await {
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

        if source.resource_field_ref.is_some() {
            anyhow::bail!("resourceFieldRef is not supported yet");
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
                if let Some(value) = self.resolve_env_var_source(source, pod, id).await? {
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
        dns: Option<DnsConfig>,
        runtime_handler: String,
        cgroup_parent: String,
        overhead: Option<LinuxContainerResources>,
    ) -> Result<String> {
        let mut rt = self.rt.clone();
        let mut config = sandbox_config(id);
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
        volumes: &HashMap<String, PathBuf>,
        pull_secrets: &[String],
        envs: &[KeyValue],
    ) -> Result<()> {
        let running_v = ContainerState::ContainerRunning as i32;
        let existing = self.list_pod_containers(sandbox_id).await?;
        let existing_ctr = existing
            .iter()
            .find(|c| c.labels.get(CTR_NAME_LABEL).map(|n| n == &container.name).unwrap_or(false));

        match restart_decision(existing_ctr.map(|c| c.state), running_v, restart_policy) {
            RestartDecision::AlreadyRunning | RestartDecision::LeaveTerminated => return Ok(()),
            RestartDecision::NeedsRestart => {
                // Not running and this pod is allowed to restart — clear the
                // stale container out (if there was one) so the create-below
                // gets a fresh one. Best-effort: if it's already gone by the
                // time we ask, or CRI won't remove it for some other reason,
                // fall through and let CreateContainer surface any real
                // problem instead of masking it here.
                if let Some(c) = existing_ctr {
                    self.bump_restart_count(sandbox_id, &container.name);
                    let mut rt = self.rt.clone();
                    let _ = rt.remove_container(RemoveContainerRequest { container_id: c.id.clone() }).await;
                }
            }
        }

        let attempt = self.restart_count(sandbox_id, &container.name);
        self.create_and_start_container(
            sandbox_id, id, container, pod_sc, volumes, pull_secrets, envs, ContainerKind::App, attempt,
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
        volumes: &HashMap<String, PathBuf>,
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
        self.create_and_start_container(
            sandbox_id, id, container, pod_sc, volumes, pull_secrets, &envs, ContainerKind::Ephemeral, 0,
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
        volumes: &HashMap<String, PathBuf>,
        pull_secrets: &[String],
        envs: &[KeyValue],
        kind: ContainerKind,
        attempt: u32,
    ) -> Result<()> {
        let image = container.image.clone().unwrap_or_default();
        let auth = self.resolve_pull_auth(&id.namespace, pull_secrets, &image).await;
        let image_spec = ImageSpec { image: image.clone(), ..Default::default() };

        // Pull the image (idempotent; containerd no-ops if present).
        let mut img = self.img.clone();
        img.pull_image(PullImageRequest {
            image: Some(image_spec.clone()),
            auth,
            sandbox_config: Some(sandbox_config(id)),
        })
        .await
        .context("pulling image")?;

        let mut mounts = build_mounts(container.volume_mounts.as_deref().unwrap_or(&[]), volumes);
        if let Some(hosts_path) = volumes.get(ETC_HOSTS_VOLUME_KEY) {
            mounts.push(Mount {
                container_path: "/etc/hosts".to_string(),
                host_path: hosts_path.to_string_lossy().into_owned(),
                readonly: false,
                ..Default::default()
            });
        }
        let linux = Some(LinuxContainerConfig {
            resources: Some(linux_resources(container.resources.as_ref())),
            security_context: Some(linux_security_context(pod_sc, container.security_context.as_ref())),
        });

        let mut rt = self.rt.clone();
        let config = ContainerConfig {
            metadata: Some(ContainerMetadata { name: container.name.clone(), attempt }),
            image: Some(image_spec),
            command: container.command.clone().unwrap_or_default(),
            args: container.args.clone().unwrap_or_default(),
            working_dir: container.working_dir.clone().unwrap_or_default(),
            envs: envs.to_vec(),
            mounts,
            labels: container_labels(id, &container.name, kind),
            log_path: format!("{}_{}.log", container.name, attempt),
            linux,
            ..Default::default()
        };

        let created = rt
            .create_container(CreateContainerRequest {
                pod_sandbox_id: sandbox_id.to_string(),
                config: Some(config),
                sandbox_config: Some(sandbox_config(id)),
            })
            .await
            .context("creating container")?
            .into_inner();

        rt.start_container(StartContainerRequest { container_id: created.container_id.clone() })
            .await
            .context("starting container")?;

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
        let spec_containers = pod.spec.as_ref().map(|s| s.containers.as_slice()).unwrap_or(&[]);

        for c in &containers {
            if c.state != running_v || c.labels.contains_key(CTR_INIT_LABEL) {
                continue;
            }
            let Some(name) = c.labels.get(CTR_NAME_LABEL) else { continue };
            if let Some(pre_stop) = spec_containers
                .iter()
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
        let mut rt = self.rt.clone();
        let resp = rt
            .container_status(ContainerStatusRequest { container_id: container_id.to_string(), verbose: false })
            .await
            .context("ContainerStatus")?
            .into_inner();
        Ok(resp.status.map(|s| s.exit_code).unwrap_or(0))
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
        volumes: &HashMap<String, PathBuf>,
        pull_secrets: &[String],
        service_env: &BTreeMap<String, Vec<u8>>,
    ) -> Result<InitProgress> {
        let running_v = ContainerState::ContainerRunning as i32;
        let exited_v = ContainerState::ContainerExited as i32;
        let existing = self.list_pod_containers(sandbox_id).await?;

        for container in init_containers {
            let existing_ctr = existing.iter().find(|c| {
                c.labels.get(CTR_NAME_LABEL).map(|n| n == &container.name).unwrap_or(false)
                    && c.labels.get(CTR_INIT_LABEL).map(|v| v == "true").unwrap_or(false)
            });
            let exit_code = match existing_ctr {
                Some(c) if c.state == exited_v => self.container_exit_code(&c.id).await?,
                _ => 0,
            };

            match init_container_decision(existing_ctr.map(|c| c.state), running_v, exited_v, exit_code, restart_policy) {
                InitContainerDecision::Create => {
                    let envs = self.resolve_container_env(pod, id, container, service_env).await?;
                    let attempt = self.restart_count(sandbox_id, &container.name);
                    self.create_and_start_container(
                        sandbox_id, id, container, pod_sc, volumes, pull_secrets, &envs, ContainerKind::Init, attempt,
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
            self.clear_restart_counts(&sandbox_id);
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

    async fn build_status(&self, sandbox_id: &str, restart_policy: &str) -> Result<RuntimeStatus> {
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
        let mut earliest_created = i64::MAX;

        let mut container_names = Vec::new();
        for c in &containers {
            let running = c.state == running_v;
            any_running |= running;
            all_exited &= c.state == exited_v;
            earliest_created = earliest_created.min(c.created_at);
            let name = c.metadata.as_ref().map(|m| m.name.clone()).unwrap_or_default();
            container_names.push((name.clone(), c.id.clone()));
            crs.push(ContainerRuntimeStatus {
                name: name.clone(),
                image: c.image.as_ref().map(|i| i.image.clone()).unwrap_or_default(),
                ready: running,
                running,
                container_id: Some(c.id.clone()),
                restart_count: self.restart_count(sandbox_id, &name),
            });
        }

        // For the default, overwhelmingly common restartPolicy: Always
        // (every Deployment, including coredns), a container exiting is
        // never terminal — ensure_container() above just restarted it — so
        // report Pending rather than Succeeded/Failed: never hand the
        // ReplicaSet controller a terminal phase for a pod that's still
        // supposed to be alive. Only restartPolicy: Never (Job-style
        // one-shot pods) can ever be genuinely done, so exit codes — one
        // extra ContainerStatus RPC per container, only paid here, not on
        // every Always/OnFailure pod's status build — are only fetched to
        // tell a real success from a real failure in that case.
        let any_failed = if all_exited && restart_policy == "Never" {
            let mut failed = false;
            for (_, container_id) in &container_names {
                if self.container_exit_code(container_id).await.unwrap_or(0) != 0 {
                    failed = true;
                    break;
                }
            }
            failed
        } else {
            false
        };
        let phase = compute_phase(any_running, all_exited, any_failed, restart_policy);

        let started_at = (earliest_created != i64::MAX && earliest_created > 0)
            .then(|| Timestamp::from_nanosecond(earliest_created as i128).ok())
            .flatten();

        Ok(RuntimeStatus {
            phase,
            message: None,
            started_at,
            pod_ip: self.pod_ip(sandbox_id).await,
            containers: crs,
            init_containers: self.build_labeled_container_statuses(sandbox_id, CTR_INIT_LABEL).await.unwrap_or_default(),
            ephemeral_containers: self
                .build_labeled_container_statuses(sandbox_id, CTR_EPHEMERAL_LABEL)
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
    /// key the *pod's* phase off).
    async fn build_labeled_container_statuses(&self, sandbox_id: &str, label: &str) -> Result<Vec<ContainerRuntimeStatus>> {
        let running_v = ContainerState::ContainerRunning as i32;
        let containers = self.list_pod_containers(sandbox_id).await?;
        Ok(containers
            .into_iter()
            .filter(|c| c.labels.contains_key(label))
            .map(|c| {
                let running = c.state == running_v;
                let name = c.metadata.as_ref().map(|m| m.name.clone()).unwrap_or_default();
                ContainerRuntimeStatus {
                    restart_count: self.restart_count(sandbox_id, &name),
                    name,
                    image: c.image.as_ref().map(|i| i.image.clone()).unwrap_or_default(),
                    ready: running,
                    running,
                    container_id: Some(c.id.clone()),
                }
            })
            .collect())
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
        // Real kubelet computes both of these before RunPodSandbox too —
        // cgroup_parent so the sandbox lands in the right QoS-scoped cgroup
        // from the start (not something CRI lets you change after the
        // fact), overhead so a RuntimeClass with declared overhead
        // (gVisor/Kata's userspace kernel cost) gets it accounted into the
        // sandbox's own resources.
        let cgroup_parent = crate::cgroup::cgroup_parent_for(crate::eviction::qos_class(pod), &id.uid);
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
                self.clear_restart_counts(&stale_id);
                self.run_sandbox(&id, dns, runtime_handler, cgroup_parent, overhead).await.context("RunPodSandbox")?
            }
            SandboxDecision::CreateFresh => {
                self.run_sandbox(&id, dns, runtime_handler, cgroup_parent, overhead).await.context("RunPodSandbox")?
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

        let pod_sc = pod.spec.as_ref().and_then(|s| s.security_context.as_ref());
        let pull_secrets: Vec<String> = pod
            .spec
            .as_ref()
            .and_then(|s| s.image_pull_secrets.as_ref())
            .map(|refs| refs.iter().map(|r| r.name.clone()).filter(|n| !n.is_empty()).collect())
            .unwrap_or_default();

        let volumes = self.resolve_volumes(pod, &id).await;
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
                            .build_labeled_container_statuses(&sandbox_id, CTR_INIT_LABEL)
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
                            .build_labeled_container_statuses(&sandbox_id, CTR_INIT_LABEL)
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
                self.ensure_container(&sandbox_id, &id, c, pod_sc, &restart_policy, &volumes, &pull_secrets, &envs)
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

        self.build_status(&sandbox_id, &restart_policy).await
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
            self.clear_restart_counts(&sandbox_id);
        }
        self.unmount_csi_volumes(pod, &id).await;
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
                Ok(Some(self.build_status(&sandbox_id, &restart_policy).await?))
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

    async fn restart_container(&self, namespace: &str, name: &str, container: &str) -> Result<()> {
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
        let _ = rt
            .stop_container(StopContainerRequest { container_id: container_id.clone(), timeout: 10 })
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

    Some(super::PodUsage {
        namespace: metadata.namespace.clone(),
        name: metadata.name.clone(),
        uid: metadata.uid.clone(),
        pod: usage_stats_from_cpu_memory(linux.cpu.as_ref(), linux.memory.as_ref()),
        containers,
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

fn sandbox_config(id: &PodId) -> PodSandboxConfig {
    // Host-network pods set the network namespace to NODE, which makes the CRI
    // runtime skip CNI entirely (no pod network to set up).
    let linux = id.host_network.then(|| LinuxPodSandboxConfig {
        security_context: Some(LinuxSandboxSecurityContext {
            namespace_options: Some(NamespaceOption {
                network: NamespaceMode::Node as i32,
                ..Default::default()
            }),
            ..Default::default()
        }),
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
        hostname: if id.host_network { String::new() } else { id.name.clone() },
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
