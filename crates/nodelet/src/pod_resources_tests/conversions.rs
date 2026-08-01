//! to_container_devices()/to_container_memory()/to_pod_resources(): the
//! pure translation from nodelet's own PodResourcesEntry DTOs into the
//! PodResources API's proto types -- unit-testable without a live gRPC
//! server or socket.
use super::*;
use crate::runtime::{ContainerResourcesEntry, PodResourcesEntry};

#[test]
fn empty_devices_produce_an_empty_list() {
    assert!(to_container_devices(&[]).is_empty());
}

#[test]
fn devices_carry_resource_name_and_ids_through_with_no_topology() {
    let devices = vec![("nvidia.com/gpu".to_string(), vec!["gpu-0".to_string(), "gpu-1".to_string()])];
    let out = to_container_devices(&devices);
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].resource_name, "nvidia.com/gpu");
    assert_eq!(out[0].device_ids, vec!["gpu-0".to_string(), "gpu-1".to_string()]);
    assert!(out[0].topology.is_none());
}

#[test]
fn empty_memory_produces_an_empty_list() {
    assert!(to_container_memory(&[]).is_empty());
}

#[test]
fn memory_carries_the_numa_node_as_topology_and_size_in_bytes() {
    let out = to_container_memory(&[(1, 4096)]);
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].memory_type, "Memory");
    assert_eq!(out[0].size, 4096);
    let topology = out[0].topology.as_ref().unwrap();
    assert_eq!(topology.nodes.len(), 1);
    assert_eq!(topology.nodes[0].id, 1);
}

#[test]
fn pod_resources_carries_namespace_name_and_every_container() {
    let entry = PodResourcesEntry {
        namespace: "default".to_string(),
        name: "my-pod".to_string(),
        containers: vec![
            ContainerResourcesEntry { name: "app".to_string(), cpu_ids: vec![0, 1], devices: vec![], memory: vec![] },
            ContainerResourcesEntry {
                name: "sidecar".to_string(),
                cpu_ids: vec![],
                devices: vec![("nvidia.com/gpu".to_string(), vec!["gpu-0".to_string()])],
                memory: vec![(0, 1024)],
            },
        ],
    };
    let pr = to_pod_resources(&entry);
    assert_eq!(pr.namespace, "default");
    assert_eq!(pr.name, "my-pod");
    assert_eq!(pr.containers.len(), 2);
    assert_eq!(pr.containers[0].cpu_ids, vec![0, 1]);
    assert!(pr.containers[0].devices.is_empty());
    assert_eq!(pr.containers[1].devices.len(), 1);
    assert_eq!(pr.containers[1].memory.len(), 1);
    // Round 74 scope: DRA claims are never surfaced here (see
    // runtime/mod.rs's ContainerResourcesEntry doc comment).
    assert!(pr.containers[0].dynamic_resources.is_empty());
}

#[test]
fn a_container_with_no_resources_at_all_still_appears_with_empty_fields() {
    let entry = PodResourcesEntry {
        namespace: "default".to_string(),
        name: "besteffort-pod".to_string(),
        containers: vec![ContainerResourcesEntry { name: "app".to_string(), cpu_ids: vec![], devices: vec![], memory: vec![] }],
    };
    let pr = to_pod_resources(&entry);
    assert_eq!(pr.containers.len(), 1);
    assert!(pr.containers[0].cpu_ids.is_empty());
    assert!(pr.containers[0].devices.is_empty());
    assert!(pr.containers[0].memory.is_empty());
}
