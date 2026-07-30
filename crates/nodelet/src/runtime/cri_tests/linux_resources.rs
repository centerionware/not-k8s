//! linux_resources(): translates Pod resources.requests/limits into CRI's
//! LinuxContainerResources. Before this, container resource requests/limits
//! were silently ignored entirely — every container ran unbounded regardless
//! of what the Pod spec asked for. These tests pin the kubelet-matching
//! formulas down (cpu.shares from requests, cpu quota/period + memory limit
//! from limits only).
use super::*;
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use std::collections::BTreeMap;

fn resources(cpu_request: Option<&str>, cpu_limit: Option<&str>, mem_limit: Option<&str>) -> ResourceRequirements {
    let mut requests = BTreeMap::new();
    if let Some(c) = cpu_request {
        requests.insert("cpu".to_string(), Quantity(c.to_string()));
    }
    let mut limits = BTreeMap::new();
    if let Some(c) = cpu_limit {
        limits.insert("cpu".to_string(), Quantity(c.to_string()));
    }
    if let Some(m) = mem_limit {
        limits.insert("memory".to_string(), Quantity(m.to_string()));
    }
    ResourceRequirements {
        requests: (!requests.is_empty()).then_some(requests),
        limits: (!limits.is_empty()).then_some(limits),
        ..Default::default()
    }
}

#[test]
fn no_resources_at_all_gets_besteffort_minimum_shares_and_unlimited_cpu_and_memory() {
    let r = linux_resources(None);
    assert_eq!(r.cpu_shares, 2);
    assert_eq!(r.cpu_quota, 0);
    assert_eq!(r.cpu_period, 0);
    assert_eq!(r.memory_limit_in_bytes, 0);
}

#[test]
fn cpu_request_of_500m_gives_512_shares() {
    let res = resources(Some("500m"), None, None);
    let r = linux_resources(Some(&res));
    assert_eq!(r.cpu_shares, 512); // 500 * 1024 / 1000
}

#[test]
fn cpu_request_of_one_whole_core_gives_1024_shares() {
    let res = resources(Some("1"), None, None);
    assert_eq!(linux_resources(Some(&res)).cpu_shares, 1024);
}

#[test]
fn cpu_shares_floor_is_two_even_for_a_tiny_request() {
    let res = resources(Some("1m"), None, None);
    assert_eq!(linux_resources(Some(&res)).cpu_shares, 2);
}

#[test]
fn cpu_limit_without_request_still_derives_shares_from_the_limit() {
    let res = resources(None, Some("2"), None);
    let r = linux_resources(Some(&res));
    assert_eq!(r.cpu_shares, 2048);
}

#[test]
fn cpu_limit_of_500m_yields_50ms_quota_over_the_100ms_period() {
    let res = resources(None, Some("500m"), None);
    let r = linux_resources(Some(&res));
    assert_eq!(r.cpu_period, 100_000);
    assert_eq!(r.cpu_quota, 50_000);
}

#[test]
fn no_cpu_limit_leaves_quota_and_period_unset() {
    let res = resources(Some("100m"), None, None);
    let r = linux_resources(Some(&res));
    assert_eq!(r.cpu_quota, 0);
    assert_eq!(r.cpu_period, 0);
}

#[test]
fn memory_limit_binary_suffix_converts_to_bytes() {
    let res = resources(None, None, Some("256Mi"));
    assert_eq!(linux_resources(Some(&res)).memory_limit_in_bytes, 256 * 1024 * 1024);
}

#[test]
fn memory_limit_decimal_suffix_converts_to_bytes() {
    let res = resources(None, None, Some("1G"));
    assert_eq!(linux_resources(Some(&res)).memory_limit_in_bytes, 1_000_000_000);
}

#[test]
fn memory_request_alone_does_not_set_a_limit() {
    // Only limits map to a CRI memory ceiling — a request alone is a
    // scheduling hint, not something CRI has a concept of enforcing.
    let mut requests = BTreeMap::new();
    requests.insert("memory".to_string(), Quantity("128Mi".to_string()));
    let res = ResourceRequirements { requests: Some(requests), ..Default::default() };
    assert_eq!(linux_resources(Some(&res)).memory_limit_in_bytes, 0);
}

#[test]
fn parse_cpu_millicores_handles_bare_decimal_cores() {
    assert_eq!(parse_cpu_millicores(&Quantity("0.5".to_string())), Some(500));
}

#[test]
fn parse_memory_bytes_handles_bare_integer_bytes() {
    assert_eq!(parse_memory_bytes(&Quantity("500000000".to_string())), Some(500_000_000));
}
