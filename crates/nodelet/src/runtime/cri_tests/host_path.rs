//! validate_host_path(): the pure(-ish, given it does real filesystem
//! syscalls) validate-and-maybe-create logic behind `hostPath.type`
//! (round 65) — a synthetic scratch directory per test, matching the
//! pattern topology_tests/read_numa_topology.rs already established.
use super::*;
use std::sync::atomic::{AtomicU64, Ordering};

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn scratch_dir() -> std::path::PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("nodelet-hostpath-test-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn no_type_at_all_performs_no_check_even_for_a_nonexistent_path() {
    let root = scratch_dir();
    let path = root.join("does-not-exist");
    assert!(validate_host_path(&path, None).is_ok());
    assert!(validate_host_path(&path, Some("")).is_ok());
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn directory_or_create_creates_a_missing_directory_with_mode_0755() {
    let root = scratch_dir();
    let path = root.join("newdir");
    assert!(validate_host_path(&path, Some("DirectoryOrCreate")).is_ok());
    let meta = std::fs::metadata(&path).unwrap();
    assert!(meta.is_dir());
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(meta.permissions().mode() & 0o777, 0o755);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn directory_or_create_accepts_an_already_existing_directory_unchanged() {
    let root = scratch_dir();
    assert!(validate_host_path(&root, Some("DirectoryOrCreate")).is_ok());
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn directory_or_create_rejects_an_existing_file() {
    let root = scratch_dir();
    let path = root.join("afile");
    std::fs::write(&path, "x").unwrap();
    assert!(validate_host_path(&path, Some("DirectoryOrCreate")).is_err());
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn directory_requires_an_already_existing_directory() {
    let root = scratch_dir();
    let missing = root.join("nope");
    assert!(validate_host_path(&missing, Some("Directory")).is_err());
    assert!(validate_host_path(&root, Some("Directory")).is_ok());
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn file_or_create_creates_a_missing_file_with_mode_0644() {
    let root = scratch_dir();
    let path = root.join("newfile");
    assert!(validate_host_path(&path, Some("FileOrCreate")).is_ok());
    let meta = std::fs::metadata(&path).unwrap();
    assert!(meta.is_file());
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(meta.permissions().mode() & 0o777, 0o644);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn file_or_create_rejects_an_existing_directory() {
    let root = scratch_dir();
    assert!(validate_host_path(&root, Some("FileOrCreate")).is_err());
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn file_requires_an_already_existing_regular_file() {
    let root = scratch_dir();
    let path = root.join("afile");
    assert!(validate_host_path(&path, Some("File")).is_err());
    std::fs::write(&path, "x").unwrap();
    assert!(validate_host_path(&path, Some("File")).is_ok());
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn socket_char_device_block_device_all_require_the_exact_kind_to_already_exist() {
    let root = scratch_dir();
    let missing = root.join("nope");
    assert!(validate_host_path(&missing, Some("Socket")).is_err());
    assert!(validate_host_path(&missing, Some("CharDevice")).is_err());
    assert!(validate_host_path(&missing, Some("BlockDevice")).is_err());
    // A plain file is the wrong kind for all three.
    let path = root.join("afile");
    std::fs::write(&path, "x").unwrap();
    assert!(validate_host_path(&path, Some("Socket")).is_err());
    assert!(validate_host_path(&path, Some("CharDevice")).is_err());
    assert!(validate_host_path(&path, Some("BlockDevice")).is_err());
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn an_unrecognized_type_is_rejected() {
    let root = scratch_dir();
    assert!(validate_host_path(&root, Some("NotARealType")).is_err());
    let _ = std::fs::remove_dir_all(&root);
}
