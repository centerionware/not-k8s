//! directory_usage_bytes(): recursive directory size, the "nodelet's own
//! materialized volumes" half of ephemeral-storage usage (Round 49).
//! Real filesystem I/O, same tempdir pattern as write_volume_dir.rs.
use super::*;

fn tmp_dir(tag: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "nodelet-test-{tag}-{}-{:?}",
        std::process::id(),
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
    ))
}

#[test]
fn nonexistent_directory_is_zero_not_an_error() {
    let dir = tmp_dir("missing");
    assert_eq!(directory_usage_bytes(&dir), 0);
}

#[test]
fn sums_file_sizes_in_a_flat_directory() {
    let dir = tmp_dir("flat");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("a"), vec![0u8; 100]).unwrap();
    std::fs::write(dir.join("b"), vec![0u8; 250]).unwrap();
    assert_eq!(directory_usage_bytes(&dir), 350);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn recurses_into_subdirectories() {
    let dir = tmp_dir("nested");
    let sub = dir.join("sub");
    std::fs::create_dir_all(&sub).unwrap();
    std::fs::write(dir.join("top"), vec![0u8; 10]).unwrap();
    std::fs::write(sub.join("deep"), vec![0u8; 20]).unwrap();
    assert_eq!(directory_usage_bytes(&dir), 30);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn empty_directory_is_zero() {
    let dir = tmp_dir("empty");
    std::fs::create_dir_all(&dir).unwrap();
    assert_eq!(directory_usage_bytes(&dir), 0);
    std::fs::remove_dir_all(&dir).ok();
}
