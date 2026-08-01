//! The PodResources API (round 74; found in round 72's fresh gap
//! re-audit): kubelet's own gRPC service exposing which CPUs/devices/
//! NUMA nodes are currently assigned to which running container. Unlike
//! every other plugin protocol in this codebase (CSI/device-plugin/DRA
//! all have nodelet dialing OUT to a plugin's socket), this is the first
//! one where nodelet is the SERVER — external device-monitoring tooling
//! (NVIDIA DCGM and similar Prometheus exporters are the real-world
//! consumers) dials IN to ask what's allocated.
//!
//! Read-only projection of state this codebase already tracks — no new
//! allocation logic. `List`/`Get` read `PodRuntime::pod_resources_snapshot()`
//! (backed by `CpuManager`/`MemoryManager`/the existing `device_allocations`
//! side table); `GetAllocatableResources` reads
//! `PodRuntime::allocatable_resources()`.
//!
//! **Deliberately not surfaced: DRA (`dynamic_resources`)** — see
//! `runtime/mod.rs`'s `ContainerResourcesEntry` doc for why (DRA claim
//! device assignments aren't kept in a queryable side table anywhere in
//! this codebase, unlike CPU/Memory/device-plugin state).
//!
//! Opt-in-by-default, matching upstream's own "always on unless disabled"
//! posture: `NODELET_POD_RESOURCES_SOCKET_PATH` has a real default (see
//! `Config`) rather than being empty-string-disabled like most of this
//! codebase's other optional features — set it to the empty string to
//! turn the server off entirely. `cri` runtime only, same as every
//! resource-manager feature this projects (CPU/Memory Manager, device
//! plugins are all `cri`-gated concepts with no mock-runtime equivalent).

use crate::config::{Config, RuntimeKind};
use crate::runtime::{PodResourcesEntry, PodRuntime};
use std::sync::Arc;
use tonic::{Request, Response, Status};
use tracing::{info, warn};

pub mod v1 {
    tonic::include_proto!("v1");
}

use v1::pod_resources_lister_server::{PodResourcesLister, PodResourcesListerServer};
use v1::{
    AllocatableResourcesRequest, AllocatableResourcesResponse, ContainerDevices, ContainerMemory, ContainerResources,
    GetPodResourcesRequest, GetPodResourcesResponse, ListPodResourcesRequest, ListPodResourcesResponse, NumaNode, PodResources,
    TopologyInfo,
};

fn to_container_devices(devices: &[(String, Vec<String>)]) -> Vec<ContainerDevices> {
    devices
        .iter()
        .map(|(resource_name, device_ids)| ContainerDevices { resource_name: resource_name.clone(), device_ids: device_ids.clone(), topology: None })
        .collect()
}

fn to_container_memory(memory: &[(u32, u64)]) -> Vec<ContainerMemory> {
    memory
        .iter()
        .map(|(node, bytes)| ContainerMemory {
            memory_type: "Memory".to_string(),
            size: *bytes,
            topology: Some(TopologyInfo { nodes: vec![NumaNode { id: i64::from(*node) }] }),
        })
        .collect()
}

fn to_pod_resources(entry: &PodResourcesEntry) -> PodResources {
    PodResources {
        name: entry.name.clone(),
        namespace: entry.namespace.clone(),
        containers: entry
            .containers
            .iter()
            .map(|c| ContainerResources {
                name: c.name.clone(),
                devices: to_container_devices(&c.devices),
                cpu_ids: c.cpu_ids.clone(),
                memory: to_container_memory(&c.memory),
                dynamic_resources: Vec::new(),
            })
            .collect(),
    }
}

struct Lister {
    runtime: Arc<dyn PodRuntime>,
}

#[tonic::async_trait]
impl PodResourcesLister for Lister {
    async fn list(&self, _req: Request<ListPodResourcesRequest>) -> Result<Response<ListPodResourcesResponse>, Status> {
        let snapshot = self.runtime.pod_resources_snapshot().await;
        Ok(Response::new(ListPodResourcesResponse { pod_resources: snapshot.iter().map(to_pod_resources).collect() }))
    }

    async fn get_allocatable_resources(
        &self,
        _req: Request<AllocatableResourcesRequest>,
    ) -> Result<Response<AllocatableResourcesResponse>, Status> {
        let a = self.runtime.allocatable_resources();
        Ok(Response::new(AllocatableResourcesResponse {
            devices: to_container_devices(&a.devices),
            cpu_ids: a.cpu_ids,
            memory: to_container_memory(&a.memory),
        }))
    }

    async fn get(&self, req: Request<GetPodResourcesRequest>) -> Result<Response<GetPodResourcesResponse>, Status> {
        let req = req.into_inner();
        let snapshot = self.runtime.pod_resources_snapshot().await;
        match snapshot.into_iter().find(|p| p.namespace == req.pod_namespace && p.name == req.pod_name) {
            Some(p) => Ok(Response::new(GetPodResourcesResponse { pod_resources: Some(to_pod_resources(&p)) })),
            None => Err(Status::not_found(format!("pod {}/{} not found", req.pod_namespace, req.pod_name))),
        }
    }
}

/// No-op if `cfg.pod_resources_socket_path` is empty (opted out) or the
/// runtime isn't `cri` (device/CPU/memory manager state is a `cri`-only
/// concept — nothing meaningful to report on the mock runtime).
pub async fn run(runtime: Arc<dyn PodRuntime>, cfg: Config) {
    if !matches!(cfg.runtime, RuntimeKind::Cri) || cfg.pod_resources_socket_path.is_empty() {
        return;
    }
    let path = std::path::Path::new(&cfg.pod_resources_socket_path);
    if let Some(parent) = path.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            warn!(error = ?e, path = %cfg.pod_resources_socket_path, "PodResources API: failed to create socket directory; server disabled for this run");
            return;
        }
    }
    // A prior run's socket file left behind (e.g. after a crash) makes
    // UnixListener::bind fail with AddrInUse — best-effort cleanup, same
    // as any other "stale state from a prior process" removal elsewhere
    // in this codebase.
    let _ = std::fs::remove_file(path);
    let listener = match tokio::net::UnixListener::bind(path) {
        Ok(l) => l,
        Err(e) => {
            warn!(error = ?e, path = %cfg.pod_resources_socket_path, "PodResources API: failed to bind Unix socket; server disabled for this run");
            return;
        }
    };
    info!(path = %cfg.pod_resources_socket_path, "PodResources API: listening");
    let incoming = tokio_stream::wrappers::UnixListenerStream::new(listener);
    let svc = Lister { runtime };
    if let Err(e) = tonic::transport::Server::builder().add_service(PodResourcesListerServer::new(svc)).serve_with_incoming(incoming).await {
        warn!(error = ?e, "PodResources API: server exited");
    }
}

#[cfg(test)]
#[path = "pod_resources_tests/conversions.rs"]
mod tests_conversions;
#[cfg(test)]
#[path = "pod_resources_tests/lister_rpc.rs"]
mod tests_lister_rpc;
