//! hugepages_capacity_map() / hugepage_size_kb_to_k8s_suffix(): Round 60's
//! Node.status.capacity["hugepages-<size>"] reporting — reads the reserved
//! pool sizes straight out of a synthetic /sys/kernel/mm/hugepages/ tree.
use super::*;
use std::sync::atomic::{AtomicU64, Ordering};

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn scratch_dir() -> std::path::PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("nodelet-hugepages-test-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn write_pool(root: &std::path::Path, size_kb: u64, nr_hugepages: u64) {
    let dir = root.join(format!("hugepages-{size_kb}kB"));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("nr_hugepages"), nr_hugepages.to_string()).unwrap();
}

#[test]
fn suffix_2048kb_is_2mi() {
    assert_eq!(hugepage_size_kb_to_k8s_suffix(2048), "2Mi");
}

#[test]
fn suffix_1048576kb_is_1gi() {
    assert_eq!(hugepage_size_kb_to_k8s_suffix(1_048_576), "1Gi");
}

#[test]
fn suffix_64kb_is_64ki_when_not_evenly_a_mib_or_gib() {
    assert_eq!(hugepage_size_kb_to_k8s_suffix(64), "64Ki");
}

#[test]
fn nonexistent_root_returns_an_empty_map_not_an_error() {
    let root = std::env::temp_dir().join("nodelet-hugepages-test-does-not-exist");
    assert!(hugepages_capacity_map(root.to_str().unwrap()).is_empty());
}

#[test]
fn reserved_2mi_pool_reports_bytes_as_count_times_page_size() {
    let root = scratch_dir();
    write_pool(&root, 2048, 4);
    let m = hugepages_capacity_map(root.to_str().unwrap());
    // 4 pages * 2048kB = 8388608 bytes.
    assert_eq!(m.get("hugepages-2Mi").unwrap().0, "8388608");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn unreserved_pool_size_with_zero_nr_hugepages_is_omitted_not_zero() {
    let root = scratch_dir();
    write_pool(&root, 2048, 0);
    let m = hugepages_capacity_map(root.to_str().unwrap());
    assert!(!m.contains_key("hugepages-2Mi"), "an unreserved pool size isn't schedulable capacity and shouldn't be advertised at all");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn multiple_reserved_pool_sizes_are_all_reported() {
    let root = scratch_dir();
    write_pool(&root, 2048, 2);
    write_pool(&root, 1_048_576, 1);
    let m = hugepages_capacity_map(root.to_str().unwrap());
    assert_eq!(m.len(), 2);
    assert_eq!(m.get("hugepages-2Mi").unwrap().0, "4194304");
    assert_eq!(m.get("hugepages-1Gi").unwrap().0, "1073741824");
    let _ = std::fs::remove_dir_all(&root);
}

// known_hugepage_suffixes(): round 124 — every size the kernel structurally
// supports, regardless of current reservation, so push_status() can null
// out a size that's dropped to zero instead of leaving a merge-patch-stale
// value behind forever.

#[test]
fn nonexistent_root_reports_no_known_sizes() {
    let root = std::env::temp_dir().join("nodelet-hugepages-test-does-not-exist-2");
    assert!(known_hugepage_suffixes(root.to_str().unwrap()).is_empty());
}

#[test]
fn an_unreserved_pool_is_still_a_known_size() {
    // The whole point: hugepages_capacity_map() omits this (nr_hugepages
    // is 0), but known_hugepage_suffixes() must still report it — the
    // directory existing at all means the kernel supports this size,
    // whether or not anything is currently reserved.
    let root = scratch_dir();
    write_pool(&root, 1_048_576, 0);
    let sizes = known_hugepage_suffixes(root.to_str().unwrap());
    assert_eq!(sizes, vec!["1Gi".to_string()]);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn reserved_and_unreserved_sizes_are_both_reported() {
    let root = scratch_dir();
    write_pool(&root, 2048, 4);
    write_pool(&root, 1_048_576, 0);
    let mut sizes = known_hugepage_suffixes(root.to_str().unwrap());
    sizes.sort();
    assert_eq!(sizes, vec!["1Gi".to_string(), "2Mi".to_string()]);
    let _ = std::fs::remove_dir_all(&root);
}
