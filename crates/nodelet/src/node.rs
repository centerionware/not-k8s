//! Node object lifecycle: registration, the Lease heartbeat, and node-status push.
//!
//! The efficiency trick lives here. The expensive `Node.status` update (capacity,
//! conditions, system info) is pushed *infrequently* (default 60s). Liveness is
//! carried by a tiny `Lease` in `kube-node-lease`, renewed cheaply (default 10s) —
//! exactly how a real kubelet decouples "I'm alive" from "here's my full status",
//! so the control plane never needs us to churn large objects to stay Ready.

use crate::config::Config;
use anyhow::Result;
use k8s_openapi::jiff::Timestamp;
use k8s_openapi::api::coordination::v1::{Lease, LeaseSpec};
use k8s_openapi::api::core::v1::{
    AttachedVolume, ContainerImage, DaemonEndpoint, Node, NodeAddress, NodeCondition, NodeDaemonEndpoints,
    NodeRuntimeHandler, NodeRuntimeHandlerFeatures, NodeSpec, NodeStatus, NodeSystemInfo, Taint,
};
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{MicroTime, ObjectMeta, Time};
use kube::api::{Patch, PatchParams};
use kube::{Api, Client};
use std::collections::BTreeMap;

const LEASE_NS: &str = "kube-node-lease";
const FIELD_MANAGER: &str = "nodelet";
const LEASE_DURATION_SECS: i32 = 40;
const CLOUDPROVIDER_TAINT_KEY: &str = "node.cloudprovider.kubernetes.io/uninitialized";

fn now_time() -> Time {
    Time(Timestamp::now())
}

fn now_micro() -> MicroTime {
    MicroTime(Timestamp::now())
}

/// Local ephemeral storage capacity (round 48; found in round 45's
/// re-audit) — real kubelet's `Node.status.capacity["ephemeral-storage"]`
/// is the total size of the filesystem backing its root dir (where
/// container writable layers/logs/emptyDir volumes actually land), read
/// via the same `statvfs(2)` call `metrics.rs`'s `DiskPressure` condition
/// already makes against `cfg.disk_path` — no new syscall plumbing needed.
/// `0` on read failure (matching `read_disk_info()`'s own "unknown, fail
/// open" contract) rather than omitting the field entirely, so it's still
/// present and simply reads as "nothing left," not silently absent.
fn ephemeral_storage_capacity_bytes(cfg: &Config) -> u64 {
    crate::metrics::read_disk_info(&cfg.disk_path).map(|d| d.total_bytes).unwrap_or(0)
}

const HUGEPAGES_SYSFS_ROOT: &str = "/sys/kernel/mm/hugepages";

/// A hugepage size in kB -> the binary-unit suffix k8s uses in its
/// `hugepages-<size>` resource name (round 60; found in round 58's
/// re-audit, the second of the 3 HugePages pieces). Real kubelet's cadvisor
/// picks the largest whole unit that divides the size evenly (2048kB ->
/// `"2Mi"`, 1048576kB -> `"1Gi"`), which is exactly what the CRI page-size
/// translation in `runtime/cri.rs` already reverses via
/// `hugepage_cri_page_size()`.
fn hugepage_size_kb_to_k8s_suffix(size_kb: u64) -> String {
    if size_kb % (1024 * 1024) == 0 {
        format!("{}Gi", size_kb / (1024 * 1024))
    } else if size_kb % 1024 == 0 {
        format!("{}Mi", size_kb / 1024)
    } else {
        format!("{size_kb}Ki")
    }
}

/// `Node.status.capacity["hugepages-<size>"]` (round 60; found in round
/// 58's re-audit) — real kubelet reads each reserved hugepage pool size
/// under `/sys/kernel/mm/hugepages/hugepages-<size>kB/`, multiplying its
/// `nr_hugepages` count by the pool's own page size. Only pools that
/// actually exist (i.e. the kernel/bootloader reserved at least one page of
/// that size) are reported, matching real kubelet: an unreserved pool size
/// isn't advertised as schedulable capacity at all. `base_path` is
/// parameterized (rather than hardcoded) purely for unit testing against a
/// synthetic sysfs tree; production always calls it with
/// `HUGEPAGES_SYSFS_ROOT`.
fn hugepages_capacity_map(base_path: &str) -> BTreeMap<String, Quantity> {
    let mut m = BTreeMap::new();
    let Ok(entries) = std::fs::read_dir(base_path) else { return m };
    for entry in entries.flatten() {
        let file_name = entry.file_name();
        let Some(name) = file_name.to_str() else { continue };
        let Some(size_kb_str) = name.strip_prefix("hugepages-").and_then(|s| s.strip_suffix("kB")) else { continue };
        let Ok(size_kb) = size_kb_str.parse::<u64>() else { continue };
        let nr_path = entry.path().join("nr_hugepages");
        let Ok(nr_str) = std::fs::read_to_string(&nr_path) else { continue };
        let Ok(nr_hugepages) = nr_str.trim().parse::<u64>() else { continue };
        if nr_hugepages == 0 {
            continue;
        }
        let bytes = size_kb * 1024 * nr_hugepages;
        let suffix = hugepage_size_kb_to_k8s_suffix(size_kb);
        m.insert(format!("hugepages-{suffix}"), Quantity(bytes.to_string()));
    }
    m
}

/// Every `"hugepages-<size>"` capacity key this kernel could *ever* report
/// — i.e. every `hugepages-<N>kB` directory under `base_path`, regardless
/// of whether it's currently reserved (`hugepages_capacity_map()` only
/// returns the reserved ones). Round 124 (found live in CI): status
/// patches go out via `Patch::Merge` (RFC 7396 JSON merge patch), which
/// only ever *adds/overwrites* keys present in the patch body — a key
/// omitted because its pool dropped to zero is NOT the same as a key
/// explicitly cleared, so a hugepage size that was ever reserved even
/// once (by this nodelet process or, per the live evidence, seemingly
/// transient kernel-level allocation activity around boot) then later
/// unreserved would keep reporting its last real byte count *forever*,
/// with no live code path ever able to produce that stale value again on
/// its own — confirmed live: `capacity.hugepages-1Gi` read a real "0"
/// while `/sys/kernel/mm/hugepages/hugepages-1048576kB/nr_hugepages`
/// simultaneously read 0 too, and `hugepages_capacity_map()` provably
/// cannot itself ever write a literal "0" (see its own unit tests). This
/// lets `push_status()` explicitly null out any known size that's
/// currently zero, so RFC 7396 actually deletes it instead of leaving
/// whatever was there.
fn known_hugepage_suffixes(base_path: &str) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(base_path) else { return Vec::new() };
    entries
        .flatten()
        .filter_map(|entry| {
            let file_name = entry.file_name();
            let name = file_name.to_str()?;
            let size_kb: u64 = name.strip_prefix("hugepages-")?.strip_suffix("kB")?.parse().ok()?;
            Some(hugepage_size_kb_to_k8s_suffix(size_kb))
        })
        .collect()
}

fn capacity_map(cfg: &Config) -> BTreeMap<String, Quantity> {
    let mut m = BTreeMap::new();
    m.insert("cpu".to_string(), Quantity(cfg.cpu_cores.to_string()));
    m.insert("memory".to_string(), Quantity(cfg.memory_bytes.to_string()));
    m.insert("pods".to_string(), Quantity(cfg.max_pods.to_string()));
    m.insert("ephemeral-storage".to_string(), Quantity(ephemeral_storage_capacity_bytes(cfg).to_string()));
    m.extend(hugepages_capacity_map(HUGEPAGES_SYSFS_ROOT));
    m
}

/// `Node.status.allocatable` = capacity minus `system-reserved` +
/// `kube-reserved` (real kubelet's formula; eviction-hard reservations
/// aren't subtracted here — nodelet's pressure-eviction thresholds already
/// serve that purpose separately, see `metrics.rs`). `pods` and
/// `ephemeral-storage` are left untouched: real kubelet doesn't reduce the
/// pod-count allocatable for cpu/memory reservations either, and this
/// project has no `--system-reserved`/`--kube-reserved`-equivalent knob
/// for ephemeral storage (round 48). Reservations only ever reduce
/// allocatable, never below zero.
fn allocatable_map(capacity: &BTreeMap<String, Quantity>, reserved_cpu_millicores: u64, reserved_memory_bytes: u64) -> BTreeMap<String, Quantity> {
    let mut m = capacity.clone();
    if let Some(cpu) = m.get_mut("cpu") {
        // capacity's cpu Quantity is always a bare whole-core count (see
        // capacity_map above), so no Ki/Mi/m suffix parsing is needed here.
        let cap_millicores = cpu.0.trim().parse::<f64>().unwrap_or(0.0) * 1000.0;
        let alloc_millicores = (cap_millicores - reserved_cpu_millicores as f64).max(0.0);
        *cpu = Quantity(format!("{}m", alloc_millicores.round() as i64));
    }
    if let Some(mem) = m.get_mut("memory") {
        let cap_bytes = mem.0.trim().parse::<u64>().unwrap_or(0);
        *mem = Quantity(cap_bytes.saturating_sub(reserved_memory_bytes).to_string());
    }
    m
}

pub(crate) fn detect_internal_ip() -> String {
    // No packets are sent; connecting a UDP socket just resolves the source
    // address the kernel would use for the default route. Works offline if a
    // default route exists; otherwise falls back to loopback.
    use std::net::UdpSocket;
    UdpSocket::bind("0.0.0.0:0")
        .and_then(|s| {
            s.connect("10.255.255.255:1")?;
            s.local_addr()
        })
        .map(|a| a.ip().to_string())
        .unwrap_or_else(|_| "127.0.0.1".to_string())
}

fn read_trim(path: &str, fallback: &str) -> String {
    std::fs::read_to_string(path)
        .map(|s| s.trim().to_string())
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| fallback.to_string())
}

fn system_info(cfg: &Config) -> NodeSystemInfo {
    let runtime_version = match cfg.runtime {
        crate::config::RuntimeKind::Mock => "mock://0.1.0".to_string(),
        crate::config::RuntimeKind::Cri => "cri://containerd".to_string(),
    };
    NodeSystemInfo {
        architecture: std::env::consts::ARCH.to_string(),
        operating_system: std::env::consts::OS.to_string(),
        kernel_version: read_trim("/proc/sys/kernel/osrelease", "unknown"),
        os_image: read_os_pretty_name(),
        container_runtime_version: runtime_version,
        kubelet_version: format!("nodelet/{}", env!("CARGO_PKG_VERSION")),
        kube_proxy_version: "n/a".to_string(),
        machine_id: read_trim("/etc/machine-id", &cfg.node_name),
        system_uuid: read_trim("/sys/class/dmi/id/product_uuid", &cfg.node_name),
        boot_id: read_trim("/proc/sys/kernel/random/boot_id", "unknown"),
        swap: None,
    }
}

fn read_os_pretty_name() -> String {
    if let Ok(os_release) = std::fs::read_to_string("/etc/os-release") {
        for line in os_release.lines() {
            if let Some(rest) = line.strip_prefix("PRETTY_NAME=") {
                return rest.trim_matches('"').to_string();
            }
        }
    }
    std::env::consts::OS.to_string()
}

fn conditions(ready: bool, pressure: &crate::metrics::Pressure) -> Vec<NodeCondition> {
    let mk = |type_: &str, status: &str, reason: &str, message: &str| NodeCondition {
        type_: type_.to_string(),
        status: status.to_string(),
        reason: Some(reason.to_string()),
        message: Some(message.to_string()),
        last_heartbeat_time: Some(now_time()),
        last_transition_time: Some(now_time()),
    };
    vec![
        mk(
            "Ready",
            if ready { "True" } else { "False" },
            "NodeletReady",
            "nodelet is posting status",
        ),
        if pressure.memory {
            mk("MemoryPressure", "True", "KubeletHasInsufficientMemory", "available memory below threshold")
        } else {
            mk("MemoryPressure", "False", "KubeletHasSufficientMemory", "sufficient memory")
        },
        if pressure.disk {
            mk("DiskPressure", "True", "KubeletHasDiskPressure", "available disk space below threshold")
        } else {
            mk("DiskPressure", "False", "KubeletHasNoDiskPressure", "no disk pressure")
        },
        if pressure.pid {
            mk("PIDPressure", "True", "KubeletHasInsufficientPID", "available PIDs below threshold")
        } else {
            mk("PIDPressure", "False", "KubeletHasSufficientPID", "sufficient PIDs")
        },
    ]
}

fn node_labels(cfg: &Config) -> BTreeMap<String, String> {
    let mut labels = BTreeMap::new();
    labels.insert("kubernetes.io/hostname".to_string(), cfg.node_name.clone());
    labels.insert("kubernetes.io/os".to_string(), std::env::consts::OS.to_string());
    labels.insert("kubernetes.io/arch".to_string(), std::env::consts::ARCH.to_string());
    labels.insert("node.kubernetes.io/instance-type".to_string(), "nodelet".to_string());
    labels.insert("nodelet.dev/managed".to_string(), "true".to_string());
    for (k, v) in &cfg.labels {
        labels.insert(k.clone(), v.clone());
    }
    labels
}

/// `volumes.kubernetes.io/controller-managed-attach-detach`.
///
/// Real kubelet writes this on its own Node whenever
/// `--enable-controller-attach-detach` is true, which is the default and is
/// the only mode nodelet implements: nodelet mounts volumes and expects
/// something else to have attached them.
///
/// It is not documentation. `kube-controller-manager`'s
/// AttachDetachController adds a node to its desired state of world *only*
/// if this annotation is present (`addNodeToDswp`), and a pod whose node is
/// not in that set is skipped entirely. Without it the controller quietly
/// does nothing at all: no VolumeAttachment is ever created for any pod on
/// this node, and every attach-required CSI volume hangs forever with
/// nodelet correctly reporting "driver requires attach but no matching
/// VolumeAttachment exists yet" — which reads as an external-attacher
/// problem and is in fact this missing string.
const CONTROLLER_MANAGED_ATTACH_DETACH: &str = "volumes.kubernetes.io/controller-managed-attach-detach";

fn node_annotations() -> BTreeMap<String, String> {
    let mut annotations = BTreeMap::new();
    annotations.insert(CONTROLLER_MANAGED_ATTACH_DETACH.to_string(), "true".to_string());
    annotations
}

/// Build the Node object (metadata + spec). Status is applied separately.
fn build_node(cfg: &Config) -> Node {
    Node {
        metadata: ObjectMeta {
            name: Some(cfg.node_name.clone()),
            labels: Some(node_labels(cfg)),
            annotations: Some(node_annotations()),
            ..Default::default()
        },
        // Deliberately no provider_id — but that alone doesn't avoid the
        // node.cloudprovider.kubernetes.io/uninitialized taint (see
        // clear_cloudprovider_taint() below): k3s's kube-controller-manager
        // is built with --cloud-provider=external baked in regardless, and
        // --disable-cloud-controller (setup-control-plane.sh passes it,
        // since there's no cloud provider on an edge device) only skips
        // starting k3s's own embedded CCM process. The cloud-node-lifecycle
        // controller still taints every newly created Node on registration,
        // expecting some CCM to clear it once initialized — confirmed for
        // real: it reappears even on a totally fresh node. Nothing was ever
        // going to clear it, so every pod stayed Unschedulable forever
        // ("1 node(s) had untolerated taint(s)") until register() below
        // clears it itself.
        spec: Some(NodeSpec::default()),
        status: None,
    }
}

/// Real kubelet's own `--node-status-max-images` default (round 33).
const NODE_STATUS_MAX_IMAGES: usize = 50;

/// Sort `images` largest-first and cap at `NODE_STATUS_MAX_IMAGES`,
/// matching real kubelet's own reporting policy — pure so the selection/
/// ordering logic is unit-testable without a runtime.
fn select_node_images(mut images: Vec<crate::runtime::NodeImage>) -> Vec<ContainerImage> {
    images.sort_by(|a, b| b.size_bytes.cmp(&a.size_bytes));
    images.truncate(NODE_STATUS_MAX_IMAGES);
    images.into_iter().map(|i| ContainerImage { names: Some(i.names), size_bytes: Some(i.size_bytes as i64) }).collect()
}

/// Real kubelet's unique-volume-name scheme for a CSI volume
/// (`pkg/volume/util`'s `GetUniqueVolumeName`, round 34) —
/// `<plugin name>/<driver>^<volume handle>`. Pure so the naming
/// convention is unit-testable without a cluster. **Unvalidated against
/// a real attach/detach controller** — see the module doc note on
/// `PodRuntime::mounted_csi_volumes()`.
fn csi_unique_volume_name(driver: &str, volume_handle: &str) -> String {
    format!("kubernetes.io/csi/{driver}^{volume_handle}")
}

/// `PodRuntime::runtime_handlers()` -> `Node.status.runtimeHandlers`
/// (round 53) — pure so the mapping is unit-testable without a cluster.
fn runtime_handlers_status(handlers: &[crate::runtime::RuntimeHandlerInfo]) -> Vec<NodeRuntimeHandler> {
    handlers
        .iter()
        .map(|h| NodeRuntimeHandler {
            name: Some(h.name.clone()),
            features: Some(NodeRuntimeHandlerFeatures {
                recursive_read_only_mounts: Some(h.recursive_read_only_mounts),
                user_namespaces: Some(h.user_namespaces),
            }),
        })
        .collect()
}

fn build_status(
    cfg: &Config,
    ready: bool,
    extra_capacity: &BTreeMap<String, u64>,
    images: Vec<crate::runtime::NodeImage>,
    mounted_csi_volumes: &[(String, String)],
    runtime_handlers: &[crate::runtime::RuntimeHandlerInfo],
) -> NodeStatus {
    let mut cap = capacity_map(cfg);
    for (name, count) in extra_capacity {
        cap.insert(name.clone(), Quantity(count.to_string()));
    }
    let pressure = crate::metrics::read_pressure(
        &cfg.disk_path,
        cfg.memory_pressure_threshold_bytes,
        cfg.disk_pressure_percent,
        cfg.pid_pressure_percent,
    );
    let allocatable = allocatable_map(
        &cap,
        cfg.system_reserved_cpu_millicores + cfg.kube_reserved_cpu_millicores,
        cfg.system_reserved_memory_bytes + cfg.kube_reserved_memory_bytes,
    );
    NodeStatus {
        capacity: Some(cap),
        allocatable: Some(allocatable),
        conditions: Some(conditions(ready, &pressure)),
        node_info: Some(system_info(cfg)),
        images: Some(select_node_images(images)),
        runtime_handlers: Some(runtime_handlers_status(runtime_handlers)),
        volumes_in_use: Some(
            mounted_csi_volumes.iter().map(|(driver, handle)| csi_unique_volume_name(driver, handle)).collect(),
        ),
        volumes_attached: Some(
            mounted_csi_volumes
                .iter()
                .map(|(driver, handle)| AttachedVolume {
                    name: csi_unique_volume_name(driver, handle),
                    device_path: String::new(), // filesystem-mounted CSI volumes have no block device path
                })
                .collect(),
        ),
        addresses: Some(vec![
            NodeAddress { type_: "InternalIP".to_string(), address: detect_internal_ip() },
            NodeAddress { type_: "Hostname".to_string(), address: cfg.node_name.clone() },
        ]),
        // The apiserver reads this to know where to proxy kubectl exec/
        // logs/attach/port-forward requests — without it, those requests
        // have no route to this node's server at all, regardless of
        // whether the server itself is running.
        daemon_endpoints: cfg.server_enabled.then(|| NodeDaemonEndpoints {
            kubelet_endpoint: Some(DaemonEndpoint { port: cfg.server_port as i32 }),
        }),
        ..Default::default()
    }
}

/// Register the node (idempotent server-side apply) and seed its status + lease.
pub async fn register(
    client: &Client,
    cfg: &Config,
    extra_capacity: &BTreeMap<String, u64>,
    images: Vec<crate::runtime::NodeImage>,
    mounted_csi_volumes: &[(String, String)],
    runtime_handlers: &[crate::runtime::RuntimeHandlerInfo],
) -> Result<()> {
    let api: Api<Node> = Api::all(client.clone());
    let pp = PatchParams::apply(FIELD_MANAGER).force();
    api.patch(&cfg.node_name, &pp, &Patch::Apply(&build_node(cfg))).await?;
    push_status(client, cfg, true, extra_capacity, images, mounted_csi_volumes, runtime_handlers).await?;
    renew_lease(client, cfg).await?;

    // k3s's cloud-node-lifecycle-controller adds this taint asynchronously
    // after node creation, not atomically with it, so a bounded retry
    // window covers the race instead of checking exactly once right after
    // apply and possibly missing it. Runs detached: it must not delay pod
    // reconciliation starting (main.rs starts the PodController immediately
    // after register() returns), and once cleared it doesn't come back —
    // this is a one-time-per-registration cleanup, not an ongoing poll.
    tokio::spawn(clear_cloudprovider_taint(client.clone(), cfg.node_name.clone()));
    Ok(())
}

/// Pure part of clear_cloudprovider_taint(): does `taints` contain `key`,
/// and what's left if it's removed. Pulled out so the "which taint is
/// removed, which are kept" logic is unit-testable independent of the
/// apiserver retry loop around it.
fn taints_without(taints: &[Taint], key: &str) -> (Vec<Taint>, bool) {
    let has_it = taints.iter().any(|t| t.key == key);
    let kept = taints.iter().filter(|t| t.key != key).cloned().collect();
    (kept, has_it)
}

async fn clear_cloudprovider_taint(client: Client, node_name: String) {
    let api: Api<Node> = Api::all(client);
    for _ in 0..10 {
        let node = match api.get(&node_name).await {
            Ok(n) => n,
            Err(_) => return, // node gone or apiserver hiccup; next registration will retry
        };
        let taints = node.spec.as_ref().and_then(|s| s.taints.as_ref());
        let (kept, has_it) = match taints {
            Some(t) => taints_without(t, CLOUDPROVIDER_TAINT_KEY),
            None => (Vec::new(), false),
        };
        if !has_it {
            // Not there (yet) — could be genuinely absent, or the
            // controller-manager just hasn't gotten to it. Keep retrying
            // for the full window rather than assuming absence on the
            // first check, which is exactly the race this guards against.
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            continue;
        }
        let patch = serde_json::json!({ "spec": { "taints": kept } });
        if let Err(e) = api.patch(&node_name, &PatchParams::default(), &Patch::Merge(&patch)).await {
            tracing::warn!(node = %node_name, error = ?e, "failed to clear cloudprovider-uninitialized taint");
        }
        return;
    }
}

/// Merge `segments` (a CSI driver's `NodeGetInfo.accessible_topology`,
/// e.g. `topology.hostpath.csi/node: debian`) onto this Node's labels —
/// the other half of real kubelet's Node Info Manager that `csi_node.rs`'s
/// own doc comment scoped out as a later follow-up, until live testing
/// showed a topology-aware `csi-provisioner` needs *both* halves: it reads
/// `topologyKeys` off `CSINode` to know which label keys matter, then reads
/// the label *values* straight off the Node object itself to build
/// `TopologyRequirement` — missing this half left it permanently failing
/// with "topologyKeys [...] were not found on any nodes" even after
/// `CSINode` carried the right keys.
///
/// Merge-patches only the given keys in (never removes existing labels,
/// same posture as `node_labels()`'s own `cfg.labels` merge) — a second
/// driver's segments call this independently and must not clobber the
/// first's.
pub async fn apply_topology_labels(client: &Client, node_name: &str, segments: &BTreeMap<String, String>) -> Result<()> {
    if segments.is_empty() {
        return Ok(());
    }
    let api: Api<Node> = Api::all(client.clone());
    let patch = serde_json::json!({ "metadata": { "labels": segments } });
    api.patch(node_name, &PatchParams::default(), &Patch::Merge(&patch)).await?;
    Ok(())
}

/// Push the (heavy, infrequent) node status.
pub async fn push_status(
    client: &Client,
    cfg: &Config,
    ready: bool,
    extra_capacity: &BTreeMap<String, u64>,
    images: Vec<crate::runtime::NodeImage>,
    mounted_csi_volumes: &[(String, String)],
    runtime_handlers: &[crate::runtime::RuntimeHandlerInfo],
) -> Result<()> {
    let api: Api<Node> = Api::all(client.clone());
    let status = build_status(cfg, ready, extra_capacity, images, mounted_csi_volumes, runtime_handlers);
    let current_keys: std::collections::BTreeSet<String> = status.capacity.iter().flatten().map(|(k, _)| k.clone()).collect();
    let mut patch = serde_json::json!({ "status": status });
    // JSON Merge Patch does not delete map keys that are absent from the new
    // value. Device-plugin deregistration removes the resource from
    // `extra_capacity`, so explicitly null any previously advertised
    // extended-resource keys that are no longer present; otherwise a plugin
    // that disappears can leave stale schedulable capacity on the Node
    // forever. Read the current status only on this infrequent full-status
    // path, not on lease renewal.
    let current = api.get(&cfg.node_name).await?;
    let mut stale_extended_resources = std::collections::BTreeSet::new();
    for resources in [current.status.as_ref().and_then(|s| s.capacity.as_ref()), current.status.as_ref().and_then(|s| s.allocatable.as_ref())]
        .into_iter()
        .flatten()
    {
        for key in resources.keys().filter(|key| key.contains('/') && !current_keys.contains(*key)) {
            stale_extended_resources.insert(key.clone());
        }
    }
    for key in stale_extended_resources {
        patch["status"]["capacity"][key.as_str()] = serde_json::Value::Null;
        patch["status"]["allocatable"][key.as_str()] = serde_json::Value::Null;
    }
    // Round 124: explicitly null out any hugepage size this kernel could
    // report but isn't reserved right now — see known_hugepage_suffixes()'s
    // own doc comment for why omission alone (what capacity_map() already
    // does by simply not including the key) isn't enough under a JSON
    // Merge Patch.
    for suffix in known_hugepage_suffixes(HUGEPAGES_SYSFS_ROOT) {
        let key = format!("hugepages-{suffix}");
        if !current_keys.contains(key.as_str()) {
            patch["status"]["capacity"][key.as_str()] = serde_json::Value::Null;
            patch["status"]["allocatable"][key.as_str()] = serde_json::Value::Null;
        }
    }
    api.patch_status(&cfg.node_name, &PatchParams::default(), &Patch::Merge(&patch)).await?;
    Ok(())
}

// Small, isolated test files — one behavior area each.
#[cfg(test)]
#[path = "node_tests/taint_filter.rs"]
mod tests_taint_filter;
#[cfg(test)]
#[path = "node_tests/capacity_map.rs"]
mod tests_capacity_map;
#[cfg(test)]
#[path = "node_tests/hugepages_capacity_map.rs"]
mod tests_hugepages_capacity_map;
#[cfg(test)]
#[path = "node_tests/node_labels.rs"]
mod tests_node_labels;
#[cfg(test)]
#[path = "node_tests/conditions.rs"]
mod tests_conditions;
#[cfg(test)]
#[path = "node_tests/read_trim.rs"]
mod tests_read_trim;
#[cfg(test)]
#[path = "node_tests/build_node.rs"]
mod tests_build_node;
#[cfg(test)]
#[path = "node_tests/allocatable_map.rs"]
mod tests_allocatable_map;
#[cfg(test)]
#[path = "node_tests/build_status.rs"]
mod tests_build_status;

/// Renew the node Lease (the cheap, frequent liveness signal).
pub async fn renew_lease(client: &Client, cfg: &Config) -> Result<()> {
    let api: Api<Lease> = Api::namespaced(client.clone(), LEASE_NS);
    let lease = Lease {
        metadata: ObjectMeta {
            name: Some(cfg.node_name.clone()),
            namespace: Some(LEASE_NS.to_string()),
            ..Default::default()
        },
        spec: Some(LeaseSpec {
            holder_identity: Some(cfg.node_name.clone()),
            lease_duration_seconds: Some(LEASE_DURATION_SECS),
            renew_time: Some(now_micro()),
            ..Default::default()
        }),
    };
    // Server-side apply creates-or-updates in one idempotent call.
    let pp = PatchParams::apply(FIELD_MANAGER).force();
    api.patch(&cfg.node_name, &pp, &Patch::Apply(&lease)).await?;
    Ok(())
}
