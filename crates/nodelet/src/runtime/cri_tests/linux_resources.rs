//! linux_resources(): translates Pod resources.requests/limits into CRI's
//! LinuxContainerResources. Before this, container resource requests/limits
//! were silently ignored entirely — every container ran unbounded regardless
//! of what the Pod spec asked for. These tests pin the kubelet-matching
//! formulas down (cpu.shares from requests, cpu quota/period + memory limit
//! from limits only).
use super::*;
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use std::collections::BTreeMap;

/// These tests are all about cpu/memory translation, not oom_score_adj
/// (see linux_resources_oom_score_adj.rs for that) — a fixed arbitrary
/// QoS/node-memory pair keeps them unaffected by round 28's new params.
fn lr(resources: Option<&ResourceRequirements>) -> LinuxContainerResources {
    linux_resources(resources, QosClass::Burstable, 4_000_000_000, 0, false)
}

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
    let r = lr(None);
    assert_eq!(r.cpu_shares, 2);
    assert_eq!(r.cpu_quota, 0);
    assert_eq!(r.cpu_period, 0);
    assert_eq!(r.memory_limit_in_bytes, 0);
}

#[test]
fn cpu_request_of_500m_gives_512_shares() {
    let res = resources(Some("500m"), None, None);
    let r = lr(Some(&res));
    assert_eq!(r.cpu_shares, 512); // 500 * 1024 / 1000
}

#[test]
fn cpu_request_of_one_whole_core_gives_1024_shares() {
    let res = resources(Some("1"), None, None);
    assert_eq!(lr(Some(&res)).cpu_shares, 1024);
}

#[test]
fn cpu_shares_floor_is_two_even_for_a_tiny_request() {
    let res = resources(Some("1m"), None, None);
    assert_eq!(lr(Some(&res)).cpu_shares, 2);
}

#[test]
fn cpu_limit_without_request_still_derives_shares_from_the_limit() {
    let res = resources(None, Some("2"), None);
    let r = lr(Some(&res));
    assert_eq!(r.cpu_shares, 2048);
}

#[test]
fn cpu_limit_of_500m_yields_50ms_quota_over_the_100ms_period() {
    let res = resources(None, Some("500m"), None);
    let r = lr(Some(&res));
    assert_eq!(r.cpu_period, 100_000);
    assert_eq!(r.cpu_quota, 50_000);
}

#[test]
fn no_cpu_limit_leaves_quota_and_period_unset() {
    let res = resources(Some("100m"), None, None);
    let r = lr(Some(&res));
    assert_eq!(r.cpu_quota, 0);
    assert_eq!(r.cpu_period, 0);
}

#[test]
fn memory_limit_binary_suffix_converts_to_bytes() {
    let res = resources(None, None, Some("256Mi"));
    assert_eq!(lr(Some(&res)).memory_limit_in_bytes, 256 * 1024 * 1024);
}

#[test]
fn memory_limit_decimal_suffix_converts_to_bytes() {
    let res = resources(None, None, Some("1G"));
    assert_eq!(lr(Some(&res)).memory_limit_in_bytes, 1_000_000_000);
}

#[test]
fn memory_request_alone_does_not_set_a_limit() {
    // Only limits map to a CRI memory ceiling — a request alone is a
    // scheduling hint, not something CRI has a concept of enforcing.
    let mut requests = BTreeMap::new();
    requests.insert("memory".to_string(), Quantity("128Mi".to_string()));
    let res = ResourceRequirements { requests: Some(requests), ..Default::default() };
    assert_eq!(lr(Some(&res)).memory_limit_in_bytes, 0);
}

#[test]
fn parse_cpu_millicores_handles_bare_decimal_cores() {
    assert_eq!(parse_cpu_millicores(&Quantity("0.5".to_string())), Some(500));
}

#[test]
fn parse_memory_bytes_handles_bare_integer_bytes() {
    assert_eq!(parse_memory_bytes(&Quantity("500000000".to_string())), Some(500_000_000));
}

// --- hugepage_limits wiring (round 59; found in round 58's re-audit) ---

#[test]
fn a_hugepages_limit_is_translated_to_the_cri_page_size_format() {
    let mut limits = BTreeMap::new();
    limits.insert("hugepages-2Mi".to_string(), Quantity("64Mi".to_string()));
    let res = ResourceRequirements { limits: Some(limits), ..Default::default() };
    let hp = lr(Some(&res)).hugepage_limits;
    assert_eq!(hp.len(), 1);
    assert_eq!(hp[0].page_size, "2MB");
    assert_eq!(hp[0].limit, 64 * 1024 * 1024);
}

#[test]
fn multiple_hugepage_sizes_all_appear() {
    let mut limits = BTreeMap::new();
    limits.insert("hugepages-2Mi".to_string(), Quantity("64Mi".to_string()));
    limits.insert("hugepages-1Gi".to_string(), Quantity("2Gi".to_string()));
    let res = ResourceRequirements { limits: Some(limits), ..Default::default() };
    let hp = lr(Some(&res)).hugepage_limits;
    assert_eq!(hp.len(), 2);
}

#[test]
fn no_hugepages_limit_produces_an_empty_list() {
    let res = resources(None, None, Some("256Mi"));
    assert!(lr(Some(&res)).hugepage_limits.is_empty());
}

// --- oom_score_adj wiring (round 28) ---

#[test]
fn guaranteed_qos_gets_the_protected_oom_score() {
    let res = resources(None, None, None);
    assert_eq!(linux_resources(Some(&res), QosClass::Guaranteed, 4_000_000_000, 0, false).oom_score_adj, -998);
}

#[test]
fn besteffort_qos_gets_the_certain_death_oom_score() {
    let res = resources(None, None, None);
    assert_eq!(linux_resources(Some(&res), QosClass::BestEffort, 4_000_000_000, 0, false).oom_score_adj, 1000);
}

#[test]
fn burstable_qos_uses_the_containers_own_memory_request_not_its_limit() {
    // memory REQUEST drives oom_score_adj (real kubelet uses Requests,
    // not Limits) — a container with only a memory limit set, no
    // request, must use 0 as the request value, same as memory_limit_in_bytes
    // already treats "request alone sets no limit" as the mirror case.
    let mut limits = BTreeMap::new();
    limits.insert("memory".to_string(), Quantity("2Gi".to_string()));
    let res = ResourceRequirements { limits: Some(limits), ..Default::default() };
    let capacity = 4i64 * 1024 * 1024 * 1024;
    // request=0 over a 4Gi node -> 1000 - 0 = 1000, clamped to 999.
    assert_eq!(linux_resources(Some(&res), QosClass::Burstable, capacity, 0, false).oom_score_adj, 999);
}
