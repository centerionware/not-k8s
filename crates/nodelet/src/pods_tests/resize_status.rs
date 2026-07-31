//! In-place pod vertical scaling status reporting (Round 43): the
//! containerStatuses[].resources/.allocatedResources fields and the
//! PodResizeInProgress condition.
use super::*;
use crate::runtime::{ContainerRuntimeStatus, Phase, RuntimeStatus};
use k8s_openapi::api::core::v1::ResourceRequirements;
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use std::collections::BTreeMap;

fn bps(rt: &RuntimeStatus) -> PodStatus {
    build_pod_status("10.0.0.1", "ns", "p", rt, None, &[], &probes::new_health_map())
}

fn requests(memory: &str) -> BTreeMap<String, Quantity> {
    let mut m = BTreeMap::new();
    m.insert("memory".to_string(), Quantity(memory.to_string()));
    m
}

fn status_with(resources: Option<ResourceRequirements>, allocated: Option<BTreeMap<String, Quantity>>) -> RuntimeStatus {
    RuntimeStatus {
        phase: Phase::Running,
        message: None,
        started_at: None,
        pod_ip: Some("10.42.0.2".to_string()),
        containers: vec![ContainerRuntimeStatus {
            name: "app".to_string(),
            image: "busybox".to_string(),
            ready: true,
            running: true,
            container_id: Some("abc123".to_string()),
            restart_count: 0,
            resources,
            allocated_resources: allocated,
            ..Default::default()
        }],
        init_containers: Vec::new(),
        ephemeral_containers: Vec::new(),
        initialized: true,
    }
}

fn resize_condition(status: &PodStatus) -> Option<&PodCondition> {
    status.conditions.as_ref().unwrap().iter().find(|c| c.type_ == "PodResizeInProgress")
}

#[test]
fn resources_and_allocated_resources_are_copied_onto_container_status() {
    let rr = ResourceRequirements { requests: Some(requests("134217728")), ..Default::default() };
    let rt = status_with(Some(rr.clone()), Some(requests("134217728")));
    let status = bps(&rt);
    let cs = &status.container_statuses.unwrap()[0];
    assert_eq!(cs.resources, Some(rr));
    assert_eq!(cs.allocated_resources, Some(requests("134217728")));
}

#[test]
fn matching_actual_and_allocated_resources_means_resize_not_in_progress() {
    let rr = ResourceRequirements { requests: Some(requests("134217728")), ..Default::default() };
    let rt = status_with(Some(rr), Some(requests("134217728")));
    let status = bps(&rt);
    assert_eq!(resize_condition(&status).unwrap().status, "False");
}

#[test]
fn mismatched_actual_and_allocated_resources_means_resize_in_progress() {
    // A resize was just requested (allocatedResources changed) but the
    // in-place UpdateContainerResources call hasn't landed yet (actual
    // resources still show the old value).
    let rr = ResourceRequirements { requests: Some(requests("134217728")), ..Default::default() };
    let rt = status_with(Some(rr), Some(requests("268435456")));
    let status = bps(&rt);
    assert_eq!(resize_condition(&status).unwrap().status, "True");
}

#[test]
fn no_resources_tracked_at_all_means_resize_not_in_progress() {
    // Init/ephemeral containers, or an app container nodelet hasn't
    // recorded resources for yet (shouldn't normally happen) — absence
    // must never be misread as "a resize is in progress."
    let rt = status_with(None, None);
    let status = bps(&rt);
    assert_eq!(resize_condition(&status).unwrap().status, "False");
}
