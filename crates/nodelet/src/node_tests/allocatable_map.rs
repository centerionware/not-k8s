//! allocatable_map(): Node.status.allocatable = capacity - (system-reserved
//! + kube-reserved). Getting this wrong either over-reports what's
//! schedulable (real risk of the node getting oversubscribed) or
//! under-reports it (wasted capacity) — either is a real correctness bug,
//! not cosmetic.
use super::*;

fn cap(cpu: &str, mem: &str, pods: &str) -> std::collections::BTreeMap<String, Quantity> {
    let mut m = std::collections::BTreeMap::new();
    m.insert("cpu".to_string(), Quantity(cpu.to_string()));
    m.insert("memory".to_string(), Quantity(mem.to_string()));
    m.insert("pods".to_string(), Quantity(pods.to_string()));
    m
}

#[test]
fn no_reservation_leaves_capacity_unchanged_except_cpu_gains_an_m_suffix() {
    // cpu is normalized to millicores even with zero reservation — still
    // numerically equal, just always expressed in the same unit.
    let m = allocatable_map(&cap("4", "8000000000", "110"), 0, 0);
    assert_eq!(m.get("cpu").unwrap().0, "4000m");
    assert_eq!(m.get("memory").unwrap().0, "8000000000");
    assert_eq!(m.get("pods").unwrap().0, "110");
}

#[test]
fn cpu_reservation_is_subtracted_in_millicores() {
    let m = allocatable_map(&cap("4", "8000000000", "110"), 500, 0);
    assert_eq!(m.get("cpu").unwrap().0, "3500m");
}

#[test]
fn memory_reservation_is_subtracted_in_bytes() {
    let m = allocatable_map(&cap("4", "8000000000", "110"), 0, 1_000_000_000);
    assert_eq!(m.get("memory").unwrap().0, "7000000000");
}

#[test]
fn reservation_larger_than_capacity_floors_at_zero_not_negative() {
    let m = allocatable_map(&cap("1", "1000", "10"), 5000, 10_000);
    assert_eq!(m.get("cpu").unwrap().0, "0m");
    assert_eq!(m.get("memory").unwrap().0, "0");
}

#[test]
fn pods_are_never_reduced_by_reservations() {
    // Real kubelet doesn't shrink the pod-count allocatable for cpu/memory
    // reservations either — only cpu and memory are ever affected.
    let m = allocatable_map(&cap("4", "8000000000", "110"), 4000, 8_000_000_000);
    assert_eq!(m.get("pods").unwrap().0, "110");
}

#[test]
fn combined_system_and_kube_reserved_both_subtract() {
    // main.rs/node.rs sum system_reserved + kube_reserved before calling
    // this — mirrored here so the arithmetic itself is pinned down.
    let system_cpu = 200u64;
    let kube_cpu = 300u64;
    let m = allocatable_map(&cap("4", "8000000000", "110"), system_cpu + kube_cpu, 0);
    assert_eq!(m.get("cpu").unwrap().0, "3500m");
}
