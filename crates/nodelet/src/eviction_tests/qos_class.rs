use super::*;
use k8s_openapi::api::core::v1::PodSpec;
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use std::collections::BTreeMap;

fn resources(requests: &[(&str, &str)], limits: &[(&str, &str)]) -> ResourceRequirements {
    let to_map = |pairs: &[(&str, &str)]| -> BTreeMap<String, Quantity> {
        pairs.iter().map(|(k, v)| (k.to_string(), Quantity(v.to_string()))).collect()
    };
    ResourceRequirements {
        requests: (!requests.is_empty()).then(|| to_map(requests)),
        limits: (!limits.is_empty()).then(|| to_map(limits)),
        ..Default::default()
    }
}

fn pod_with_container(resources: ResourceRequirements) -> Pod {
    Pod {
        spec: Some(PodSpec {
            containers: vec![Container { name: "app".to_string(), resources: Some(resources), ..Default::default() }],
            ..Default::default()
        }),
        ..Default::default()
    }
}

#[test]
fn no_containers_is_besteffort() {
    let pod = Pod { spec: Some(PodSpec::default()), ..Default::default() };
    assert_eq!(qos_class(&pod), QosClass::BestEffort);
}

#[test]
fn no_requests_or_limits_at_all_is_besteffort() {
    let pod = pod_with_container(ResourceRequirements::default());
    assert_eq!(qos_class(&pod), QosClass::BestEffort);
}

#[test]
fn equal_requests_and_limits_for_both_cpu_and_memory_is_guaranteed() {
    let pod = pod_with_container(resources(&[("cpu", "500m"), ("memory", "128Mi")], &[("cpu", "500m"), ("memory", "128Mi")]));
    assert_eq!(qos_class(&pod), QosClass::Guaranteed);
}

#[test]
fn equal_requests_and_limits_with_different_but_equivalent_units_is_guaranteed() {
    // 1000m == 1 core, 1024Ki == 1Mi — semantic equality, not string equality.
    let pod = pod_with_container(resources(&[("cpu", "1000m"), ("memory", "1024Ki")], &[("cpu", "1"), ("memory", "1Mi")]));
    assert_eq!(qos_class(&pod), QosClass::Guaranteed);
}

#[test]
fn limit_without_matching_request_value_is_burstable() {
    let pod = pod_with_container(resources(&[("cpu", "100m"), ("memory", "64Mi")], &[("cpu", "500m"), ("memory", "128Mi")]));
    assert_eq!(qos_class(&pod), QosClass::Burstable);
}

#[test]
fn only_requests_set_no_limits_is_burstable() {
    let pod = pod_with_container(resources(&[("cpu", "100m"), ("memory", "64Mi")], &[]));
    assert_eq!(qos_class(&pod), QosClass::Burstable);
}

#[test]
fn missing_one_resource_type_is_burstable_not_guaranteed() {
    // Guaranteed requires BOTH cpu and memory to have matching request==limit.
    let pod = pod_with_container(resources(&[("cpu", "500m")], &[("cpu", "500m")]));
    assert_eq!(qos_class(&pod), QosClass::Burstable);
}

#[test]
fn multiple_containers_all_guaranteed_is_guaranteed() {
    let c1 = Container {
        name: "app".to_string(),
        resources: Some(resources(&[("cpu", "500m"), ("memory", "128Mi")], &[("cpu", "500m"), ("memory", "128Mi")])),
        ..Default::default()
    };
    let c2 = Container {
        name: "sidecar".to_string(),
        resources: Some(resources(&[("cpu", "100m"), ("memory", "32Mi")], &[("cpu", "100m"), ("memory", "32Mi")])),
        ..Default::default()
    };
    let pod = Pod {
        spec: Some(PodSpec { containers: vec![c1, c2], ..Default::default() }),
        ..Default::default()
    };
    assert_eq!(qos_class(&pod), QosClass::Guaranteed);
}

#[test]
fn one_besteffort_container_among_guaranteed_ones_is_burstable() {
    let c1 = Container {
        name: "app".to_string(),
        resources: Some(resources(&[("cpu", "500m"), ("memory", "128Mi")], &[("cpu", "500m"), ("memory", "128Mi")])),
        ..Default::default()
    };
    let c2 = Container { name: "sidecar".to_string(), resources: None, ..Default::default() };
    let pod = Pod {
        spec: Some(PodSpec { containers: vec![c1, c2], ..Default::default() }),
        ..Default::default()
    };
    assert_eq!(qos_class(&pod), QosClass::Burstable);
}

#[test]
fn init_containers_count_toward_qos_too() {
    let app = Container {
        name: "app".to_string(),
        resources: Some(resources(&[("cpu", "500m"), ("memory", "128Mi")], &[("cpu", "500m"), ("memory", "128Mi")])),
        ..Default::default()
    };
    let init = Container { name: "setup".to_string(), resources: None, ..Default::default() };
    let pod = Pod {
        spec: Some(PodSpec { containers: vec![app], init_containers: Some(vec![init]), ..Default::default() }),
        ..Default::default()
    };
    assert_eq!(qos_class(&pod), QosClass::Burstable, "a besteffort init container disqualifies Guaranteed too");
}

#[test]
fn as_str_matches_the_real_kubernetes_api_constants() {
    // Round 55: PodStatus.qosClass reads these exact strings.
    assert_eq!(QosClass::BestEffort.as_str(), "BestEffort");
    assert_eq!(QosClass::Burstable.as_str(), "Burstable");
    assert_eq!(QosClass::Guaranteed.as_str(), "Guaranteed");
}
