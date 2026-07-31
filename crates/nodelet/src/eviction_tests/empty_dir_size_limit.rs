//! empty_dir_size_limits()/first_empty_dir_over_limit() (round 67; found
//! in round 65's fresh gap re-audit) — per-volume emptyDir.sizeLimit
//! violation, distinct from the whole-pod ephemeral-storage limit.
use super::*;
use k8s_openapi::api::core::v1::{EmptyDirVolumeSource, PodSpec, Volume};
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use std::collections::HashMap;

fn empty_dir_volume(name: &str, medium: Option<&str>, size_limit: Option<&str>) -> Volume {
    Volume {
        name: name.to_string(),
        empty_dir: Some(EmptyDirVolumeSource {
            medium: medium.map(str::to_string),
            size_limit: size_limit.map(|s| Quantity(s.to_string())),
        }),
        ..Default::default()
    }
}

fn pod_with_volumes(volumes: Vec<Volume>) -> Pod {
    Pod { spec: Some(PodSpec { volumes: Some(volumes), ..Default::default() }), ..Default::default() }
}

#[test]
fn no_volumes_at_all_yields_no_limits() {
    let pod = Pod::default();
    assert!(empty_dir_size_limits(&pod).is_empty());
}

#[test]
fn a_disk_backed_empty_dir_with_a_size_limit_is_reported() {
    let pod = pod_with_volumes(vec![empty_dir_volume("cache", None, Some("100Mi"))]);
    let limits = empty_dir_size_limits(&pod);
    assert_eq!(limits, vec![("cache".to_string(), 100 * 1024 * 1024)]);
}

#[test]
fn an_empty_dir_with_no_size_limit_is_not_reported() {
    let pod = pod_with_volumes(vec![empty_dir_volume("cache", None, None)]);
    assert!(empty_dir_size_limits(&pod).is_empty());
}

#[test]
fn a_memory_medium_empty_dir_is_never_reported_even_with_a_size_limit() {
    // tmpfs sizeLimit is already a real kernel-enforced cap at mount
    // time (round 30) — nothing for periodic-measurement eviction to add.
    let pod = pod_with_volumes(vec![empty_dir_volume("ramdisk", Some("Memory"), Some("50Mi"))]);
    assert!(empty_dir_size_limits(&pod).is_empty());
}

#[test]
fn a_hugepages_medium_empty_dir_is_never_reported_either() {
    let pod = pod_with_volumes(vec![empty_dir_volume("huge", Some("HugePages-2Mi"), Some("4Mi"))]);
    assert!(empty_dir_size_limits(&pod).is_empty());
}

#[test]
fn non_empty_dir_volumes_are_ignored() {
    let mut pod = pod_with_volumes(vec![]);
    pod.spec.as_mut().unwrap().volumes =
        Some(vec![Volume { name: "cm".to_string(), config_map: Some(Default::default()), ..Default::default() }]);
    assert!(empty_dir_size_limits(&pod).is_empty());
}

#[test]
fn multiple_empty_dir_volumes_each_get_their_own_entry() {
    let pod = pod_with_volumes(vec![empty_dir_volume("a", None, Some("10Mi")), empty_dir_volume("b", None, Some("20Mi"))]);
    let limits = empty_dir_size_limits(&pod);
    assert_eq!(limits.len(), 2);
    assert!(limits.contains(&("a".to_string(), 10 * 1024 * 1024)));
    assert!(limits.contains(&("b".to_string(), 20 * 1024 * 1024)));
}

// --- first_empty_dir_over_limit() ---

#[test]
fn no_limits_at_all_never_violates() {
    let usage = HashMap::from([("cache".to_string(), 1_000_000u64)]);
    assert_eq!(first_empty_dir_over_limit(&[], &usage), None);
}

#[test]
fn usage_over_its_own_limit_is_a_violation() {
    let limits = vec![("cache".to_string(), 100u64)];
    let usage = HashMap::from([("cache".to_string(), 200u64)]);
    assert_eq!(first_empty_dir_over_limit(&limits, &usage), Some("cache".to_string()));
}

#[test]
fn usage_at_or_under_the_limit_is_not_a_violation() {
    let limits = vec![("cache".to_string(), 100u64)];
    assert_eq!(first_empty_dir_over_limit(&limits, &HashMap::from([("cache".to_string(), 100u64)])), None);
    assert_eq!(first_empty_dir_over_limit(&limits, &HashMap::from([("cache".to_string(), 50u64)])), None);
}

#[test]
fn unmeasured_usage_never_violates() {
    let limits = vec![("cache".to_string(), 100u64)];
    assert_eq!(first_empty_dir_over_limit(&limits, &HashMap::new()), None);
}

#[test]
fn only_the_violating_volume_among_several_is_returned() {
    let limits = vec![("a".to_string(), 100u64), ("b".to_string(), 100u64)];
    let usage = HashMap::from([("a".to_string(), 50u64), ("b".to_string(), 200u64)]);
    assert_eq!(first_empty_dir_over_limit(&limits, &usage), Some("b".to_string()));
}
