//! Points `enforce_node_allocatable` at a throwaway directory instead of a
//! real `/sys/fs/cgroup` — this can't validate real kernel cgroup v2
//! semantics (no root, no guarantee this sandbox even has a writable
//! cgroup v2 tree), but it does pin down the file layout and content this
//! function is responsible for getting right, and that it never panics
//! or errors out loud when the "cgroup.subtree_control" write (which a
//! plain directory obviously can't satisfy) fails.
use super::*;
use std::sync::atomic::{AtomicU64, Ordering};

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn scratch_dir() -> std::path::PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("nodelet-cgroup-test-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn creates_the_kubepods_directory() {
    let root = scratch_dir();
    enforce_node_allocatable(root.to_str().unwrap(), 2000, 1_073_741_824);
    assert!(root.join(CGROUP_ROOT_NAME).is_dir());
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn writes_cpu_max_with_the_expected_content() {
    let root = scratch_dir();
    enforce_node_allocatable(root.to_str().unwrap(), 2000, 1_073_741_824);
    let cpu_max = std::fs::read_to_string(root.join(CGROUP_ROOT_NAME).join("cpu.max")).unwrap();
    assert_eq!(cpu_max, "200000 100000");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn writes_memory_max_with_the_expected_content() {
    let root = scratch_dir();
    enforce_node_allocatable(root.to_str().unwrap(), 2000, 1_073_741_824);
    let mem_max = std::fs::read_to_string(root.join(CGROUP_ROOT_NAME).join("memory.max")).unwrap();
    assert_eq!(mem_max, "1073741824");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn zero_allocatable_writes_max_unlimited() {
    let root = scratch_dir();
    enforce_node_allocatable(root.to_str().unwrap(), 0, 0);
    assert_eq!(std::fs::read_to_string(root.join(CGROUP_ROOT_NAME).join("cpu.max")).unwrap(), "max");
    assert_eq!(std::fs::read_to_string(root.join(CGROUP_ROOT_NAME).join("memory.max")).unwrap(), "max");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn an_unwritable_root_does_not_panic() {
    // Pointing at a path that can never be created (a file, not a
    // directory, in the way) exercises the create_dir_all failure branch —
    // must log and return, not panic.
    let root = scratch_dir();
    let blocker = root.join("blocked");
    std::fs::write(&blocker, b"not a directory").unwrap();
    enforce_node_allocatable(blocker.to_str().unwrap(), 1000, 1000);
    let _ = std::fs::remove_dir_all(&root);
}
