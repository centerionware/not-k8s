//! resource_list_to_linux_resources(): translates spec.overhead (a flat
//! ResourceList, not a request/limit pair) into CRI's LinuxContainerResources
//! for LinuxPodSandboxConfig.overhead — the RuntimeClass Overhead field.
use super::*;
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use std::collections::BTreeMap;

fn list(cpu: Option<&str>, memory: Option<&str>) -> BTreeMap<String, Quantity> {
    let mut m = BTreeMap::new();
    if let Some(c) = cpu {
        m.insert("cpu".to_string(), Quantity(c.to_string()));
    }
    if let Some(mem) = memory {
        m.insert("memory".to_string(), Quantity(mem.to_string()));
    }
    m
}

#[test]
fn empty_overhead_yields_besteffort_minimum_shares_and_unlimited() {
    let r = resource_list_to_linux_resources(&list(None, None));
    assert_eq!(r.cpu_shares, 2);
    assert_eq!(r.cpu_quota, 0);
    assert_eq!(r.cpu_period, 0);
    assert_eq!(r.memory_limit_in_bytes, 0);
}

#[test]
fn cpu_overhead_sets_both_shares_and_quota_unlike_a_plain_request() {
    // Overhead has no request/limit distinction — a value here must drive
    // cpu_quota too, unlike linux_resources() where a bare request never
    // sets a quota on its own.
    let r = resource_list_to_linux_resources(&list(Some("250m"), None));
    assert_eq!(r.cpu_shares, 256); // 250 * 1024 / 1000
    assert_eq!(r.cpu_period, 100_000);
    assert_eq!(r.cpu_quota, 25_000);
}

#[test]
fn memory_overhead_becomes_the_memory_limit() {
    let r = resource_list_to_linux_resources(&list(None, Some("64Mi")));
    assert_eq!(r.memory_limit_in_bytes, 64 * 1024 * 1024);
}

#[test]
fn both_cpu_and_memory_overhead_are_applied_together() {
    let r = resource_list_to_linux_resources(&list(Some("100m"), Some("32Mi")));
    assert_eq!(r.cpu_shares, 102); // 100 * 1024 / 1000, truncated
    assert_eq!(r.memory_limit_in_bytes, 32 * 1024 * 1024);
}
