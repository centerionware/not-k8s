//! Lister's List/GetAllocatableResources/Get RPC handlers, exercised
//! against the mock runtime (which reports no CPU/Memory/device-manager
//! state at all -- those are `cri`-only concepts, see `PodRuntime`'s
//! default trait impls). Proves the gRPC request/response wiring itself
//! is sound; a real allocation-bearing response is covered by
//! `conversions.rs`'s pure-function tests instead, since exercising a
//! real CRI-backed CriRuntime here would need a live containerd socket.
use super::*;
use crate::runtime::mock::MockRuntime;
use std::sync::Arc;

fn lister() -> Lister {
    Lister { runtime: Arc::new(MockRuntime::new()) }
}

#[tokio::test]
async fn list_against_a_runtime_with_no_pods_returns_an_empty_list() {
    let resp = lister().list(Request::new(ListPodResourcesRequest {})).await.unwrap();
    assert!(resp.into_inner().pod_resources.is_empty());
}

#[tokio::test]
async fn get_allocatable_resources_against_a_runtime_with_no_managers_returns_empty_fields() {
    let resp = lister().get_allocatable_resources(Request::new(AllocatableResourcesRequest {})).await.unwrap();
    let inner = resp.into_inner();
    assert!(inner.cpu_ids.is_empty());
    assert!(inner.devices.is_empty());
    assert!(inner.memory.is_empty());
}

#[tokio::test]
async fn get_for_a_pod_that_does_not_exist_returns_not_found() {
    let req = GetPodResourcesRequest { pod_name: "nope".to_string(), pod_namespace: "default".to_string() };
    let err = lister().get(Request::new(req)).await.unwrap_err();
    assert_eq!(err.code(), tonic::Code::NotFound);
}
