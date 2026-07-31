//! ephemeral_storage_limit_bytes()/exceeds_ephemeral_storage_limit()
//! (Round 49; the deferred half of round 48's arc) — per-pod
//! ephemeral-storage limit violation, checked independently of general
//! node-pressure-based eviction.
use super::*;
use k8s_openapi::api::core::v1::PodSpec;
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use std::collections::BTreeMap;

fn resources(limits: &[(&str, &str)]) -> ResourceRequirements {
    let map: BTreeMap<String, Quantity> = limits.iter().map(|(k, v)| (k.to_string(), Quantity(v.to_string()))).collect();
    ResourceRequirements { limits: (!map.is_empty()).then_some(map), ..Default::default() }
}

fn pod_with_containers(resources_per_container: &[ResourceRequirements]) -> Pod {
    Pod {
        spec: Some(PodSpec {
            containers: resources_per_container
                .iter()
                .enumerate()
                .map(|(i, r)| Container { name: format!("c{i}"), resources: Some(r.clone()), ..Default::default() })
                .collect(),
            ..Default::default()
        }),
        ..Default::default()
    }
}

#[test]
fn no_container_setting_a_limit_means_none() {
    let pod = pod_with_containers(&[resources(&[])]);
    assert_eq!(ephemeral_storage_limit_bytes(&pod), None);
}

#[test]
fn a_single_containers_limit_is_reported_in_bytes() {
    let pod = pod_with_containers(&[resources(&[("ephemeral-storage", "1Gi")])]);
    assert_eq!(ephemeral_storage_limit_bytes(&pod), Some(1024 * 1024 * 1024));
}

#[test]
fn multiple_containers_limits_are_summed() {
    let pod = pod_with_containers(&[resources(&[("ephemeral-storage", "500Mi")]), resources(&[("ephemeral-storage", "500Mi")])]);
    assert_eq!(ephemeral_storage_limit_bytes(&pod), Some(1024 * 1024 * 1000));
}

#[test]
fn an_explicit_zero_limit_is_some_zero_not_none() {
    // A pod that deliberately sets limit "0" would violate it immediately
    // with any real usage at all — distinct from "no limit configured."
    let pod = pod_with_containers(&[resources(&[("ephemeral-storage", "0")])]);
    assert_eq!(ephemeral_storage_limit_bytes(&pod), Some(0));
}

#[test]
fn usage_over_limit_exceeds() {
    assert!(exceeds_ephemeral_storage_limit(Some(200), Some(100)));
}

#[test]
fn usage_under_or_equal_to_limit_does_not_exceed() {
    assert!(!exceeds_ephemeral_storage_limit(Some(100), Some(100)));
    assert!(!exceeds_ephemeral_storage_limit(Some(50), Some(100)));
}

#[test]
fn unknown_usage_or_unknown_limit_never_exceeds() {
    assert!(!exceeds_ephemeral_storage_limit(None, Some(100)));
    assert!(!exceeds_ephemeral_storage_limit(Some(200), None));
    assert!(!exceeds_ephemeral_storage_limit(None, None));
}
