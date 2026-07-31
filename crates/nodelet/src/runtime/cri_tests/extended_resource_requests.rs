//! extended_resource_requests(): the pure extraction behind device-plugin
//! resource detection in create_and_start_container() — every non-cpu/
//! memory key in a container's resources.limits, as (name, count).
use super::*;
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use std::collections::BTreeMap;

fn limits(pairs: &[(&str, &str)]) -> BTreeMap<String, Quantity> {
    pairs.iter().map(|(k, v)| (k.to_string(), Quantity(v.to_string()))).collect()
}

#[test]
fn no_limits_at_all_returns_empty() {
    assert!(extended_resource_requests(None).is_empty());
}

#[test]
fn cpu_and_memory_are_excluded() {
    let m = limits(&[("cpu", "500m"), ("memory", "128Mi")]);
    assert!(extended_resource_requests(Some(&m)).is_empty());
}

#[test]
fn an_extended_resource_is_extracted_with_its_count() {
    let m = limits(&[("nvidia.com/gpu", "1")]);
    let got = extended_resource_requests(Some(&m));
    assert_eq!(got, vec![("nvidia.com/gpu".to_string(), 1)]);
}

#[test]
fn multiple_extended_resources_alongside_cpu_and_memory() {
    let m = limits(&[("cpu", "1"), ("memory", "1Gi"), ("nvidia.com/gpu", "2"), ("example.com/fpga", "1")]);
    let mut got = extended_resource_requests(Some(&m));
    got.sort();
    assert_eq!(got, vec![("example.com/fpga".to_string(), 1), ("nvidia.com/gpu".to_string(), 2)]);
}

#[test]
fn a_garbage_quantity_value_is_dropped_not_a_panic() {
    let m = limits(&[("nvidia.com/gpu", "not-a-number")]);
    assert!(extended_resource_requests(Some(&m)).is_empty());
}

#[test]
fn fractional_values_round_to_the_nearest_whole_count() {
    // Extended resources are always whole units in practice, but this
    // shouldn't panic or truncate oddly if one ever isn't.
    let m = limits(&[("nvidia.com/gpu", "1.6")]);
    assert_eq!(extended_resource_requests(Some(&m)), vec![("nvidia.com/gpu".to_string(), 2)]);
}
