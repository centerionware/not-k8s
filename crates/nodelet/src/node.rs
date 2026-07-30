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
    Node, NodeAddress, NodeCondition, NodeSpec, NodeStatus, NodeSystemInfo,
};
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{MicroTime, ObjectMeta, Time};
use kube::api::{Patch, PatchParams};
use kube::{Api, Client};
use std::collections::BTreeMap;

const LEASE_NS: &str = "kube-node-lease";
const FIELD_MANAGER: &str = "nodelet";
const LEASE_DURATION_SECS: i32 = 40;

fn now_time() -> Time {
    Time(Timestamp::now())
}

fn now_micro() -> MicroTime {
    MicroTime(Timestamp::now())
}

fn capacity_map(cfg: &Config) -> BTreeMap<String, Quantity> {
    let mut m = BTreeMap::new();
    m.insert("cpu".to_string(), Quantity(cfg.cpu_cores.to_string()));
    m.insert("memory".to_string(), Quantity(cfg.memory_bytes.to_string()));
    m.insert("pods".to_string(), Quantity(cfg.max_pods.to_string()));
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

fn conditions(ready: bool) -> Vec<NodeCondition> {
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
        mk("MemoryPressure", "False", "KubeletHasSufficientMemory", "sufficient memory"),
        mk("DiskPressure", "False", "KubeletHasNoDiskPressure", "no disk pressure"),
        mk("PIDPressure", "False", "KubeletHasSufficientPID", "sufficient PIDs"),
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

/// Build the Node object (metadata + spec). Status is applied separately.
fn build_node(cfg: &Config) -> Node {
    Node {
        metadata: ObjectMeta {
            name: Some(cfg.node_name.clone()),
            labels: Some(node_labels(cfg)),
            ..Default::default()
        },
        // Deliberately no provider_id. Setting one is exactly what tells
        // Kubernetes' node-lifecycle-controller that a cloud-controller-
        // manager will show up to initialize this node — it responds by
        // adding a node.cloudprovider.kubernetes.io/uninitialized:NoSchedule
        // taint until one does. There's no cloud provider here at all
        // (setup-control-plane.sh passes --disable-cloud-controller on
        // purpose, for exactly this reason: an edge device), so nothing
        // was ever going to clear it — every pod would stay Unschedulable
        // forever. Confirmed for real: this was the actual root cause of
        // pods stuck Pending with "1 node(s) had untolerated taint(s)".
        spec: Some(NodeSpec::default()),
        status: None,
    }
}

fn build_status(cfg: &Config, ready: bool) -> NodeStatus {
    let cap = capacity_map(cfg);
    NodeStatus {
        capacity: Some(cap.clone()),
        allocatable: Some(cap),
        conditions: Some(conditions(ready)),
        node_info: Some(system_info(cfg)),
        addresses: Some(vec![
            NodeAddress { type_: "InternalIP".to_string(), address: detect_internal_ip() },
            NodeAddress { type_: "Hostname".to_string(), address: cfg.node_name.clone() },
        ]),
        ..Default::default()
    }
}

/// Register the node (idempotent server-side apply) and seed its status + lease.
pub async fn register(client: &Client, cfg: &Config) -> Result<()> {
    let api: Api<Node> = Api::all(client.clone());
    let pp = PatchParams::apply(FIELD_MANAGER).force();
    api.patch(&cfg.node_name, &pp, &Patch::Apply(&build_node(cfg))).await?;
    push_status(client, cfg, true).await?;
    renew_lease(client, cfg).await?;
    Ok(())
}

/// Push the (heavy, infrequent) node status.
pub async fn push_status(client: &Client, cfg: &Config, ready: bool) -> Result<()> {
    let api: Api<Node> = Api::all(client.clone());
    let status = build_status(cfg, ready);
    let patch = serde_json::json!({ "status": status });
    api.patch_status(&cfg.node_name, &PatchParams::default(), &Patch::Merge(&patch)).await?;
    Ok(())
}

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
