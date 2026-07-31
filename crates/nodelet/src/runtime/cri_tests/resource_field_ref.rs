//! resolve_resource_field_ref()/format_resource_field_value(): env
//! resourceFieldRef (Round 44; found in round 35's re-audit). Previously
//! this branch unconditionally bail!ed "not supported yet".
use super::*;
use k8s_openapi::api::core::v1::ResourceFieldSelector;

fn resources(cpu_request: Option<&str>, cpu_limit: Option<&str>, mem_request: Option<&str>, mem_limit: Option<&str>) -> ResourceRequirements {
    let mut requests = BTreeMap::new();
    if let Some(v) = cpu_request {
        requests.insert("cpu".to_string(), Quantity(v.to_string()));
    }
    if let Some(v) = mem_request {
        requests.insert("memory".to_string(), Quantity(v.to_string()));
    }
    let mut limits = BTreeMap::new();
    if let Some(v) = cpu_limit {
        limits.insert("cpu".to_string(), Quantity(v.to_string()));
    }
    if let Some(v) = mem_limit {
        limits.insert("memory".to_string(), Quantity(v.to_string()));
    }
    ResourceRequirements {
        requests: (!requests.is_empty()).then_some(requests),
        limits: (!limits.is_empty()).then_some(limits),
        ..Default::default()
    }
}

fn field_ref(resource: &str, divisor: Option<&str>) -> ResourceFieldSelector {
    ResourceFieldSelector {
        container_name: None,
        divisor: divisor.map(|d| Quantity(d.to_string())),
        resource: resource.to_string(),
    }
}

#[test]
fn format_resource_field_value_rounds_up_to_whole_divisor_units() {
    assert_eq!(format_resource_field_value(1500, 1000), "2"); // 1.5 cores -> ceil to 2
    assert_eq!(format_resource_field_value(2000, 1000), "2"); // exact -> 2
    assert_eq!(format_resource_field_value(1, 1000), "1"); // tiny nonzero -> still rounds up to 1
    assert_eq!(format_resource_field_value(0, 1000), "0");
}

#[test]
fn limits_cpu_with_no_divisor_reports_whole_cores_rounded_up() {
    let r = resources(None, Some("1500m"), None, None);
    let value = resolve_resource_field_ref(&field_ref("limits.cpu", None), Some(&r), 4000, 4 * 1024 * 1024 * 1024).unwrap();
    assert_eq!(value, "2");
}

#[test]
fn limits_cpu_with_millicore_divisor_reports_exact_millicores() {
    let r = resources(None, Some("1500m"), None, None);
    let value = resolve_resource_field_ref(&field_ref("limits.cpu", Some("1m")), Some(&r), 4000, 4 * 1024 * 1024 * 1024).unwrap();
    assert_eq!(value, "1500");
}

#[test]
fn limits_cpu_unset_falls_back_to_node_capacity() {
    let r = resources(None, None, None, None);
    let value = resolve_resource_field_ref(&field_ref("limits.cpu", None), Some(&r), 4000, 4 * 1024 * 1024 * 1024).unwrap();
    assert_eq!(value, "4"); // 4000 millicores node capacity -> 4 whole cores
}

#[test]
fn requests_cpu_falls_back_to_limit_then_node_capacity() {
    let with_limit_only = resources(None, Some("2000m"), None, None);
    assert_eq!(
        resolve_resource_field_ref(&field_ref("requests.cpu", None), Some(&with_limit_only), 4000, 0).unwrap(),
        "2"
    );
    let neither = resources(None, None, None, None);
    assert_eq!(
        resolve_resource_field_ref(&field_ref("requests.cpu", None), Some(&neither), 4000, 0).unwrap(),
        "4"
    );
}

#[test]
fn limits_memory_with_mebibyte_divisor_matches_the_jvm_heap_sizing_use_case() {
    let r = resources(None, None, None, Some("536870912")); // 512Mi
    let value = resolve_resource_field_ref(&field_ref("limits.memory", Some("1Mi")), Some(&r), 0, 0).unwrap();
    assert_eq!(value, "512");
}

#[test]
fn limits_memory_unset_falls_back_to_node_capacity() {
    let r = resources(None, None, None, None);
    let value = resolve_resource_field_ref(&field_ref("limits.memory", None), Some(&r), 0, 268_435_456).unwrap();
    assert_eq!(value, "268435456");
}

#[test]
fn ephemeral_storage_resolves_to_zero_not_an_error() {
    let r = resources(None, None, None, None);
    assert_eq!(resolve_resource_field_ref(&field_ref("limits.ephemeral-storage", None), Some(&r), 0, 0).unwrap(), "0");
    assert_eq!(resolve_resource_field_ref(&field_ref("requests.ephemeral-storage", None), Some(&r), 0, 0).unwrap(), "0");
}

#[test]
fn unsupported_resource_name_is_a_real_error_not_silently_zero() {
    let r = resources(None, None, None, None);
    assert!(resolve_resource_field_ref(&field_ref("limits.hugepages-2Mi", None), Some(&r), 0, 0).is_err());
}

#[test]
fn no_resources_at_all_falls_back_to_node_capacity() {
    let value = resolve_resource_field_ref(&field_ref("limits.cpu", None), None, 4000, 0).unwrap();
    assert_eq!(value, "4");
}
