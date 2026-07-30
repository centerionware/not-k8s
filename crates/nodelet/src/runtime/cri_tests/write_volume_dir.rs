//! write_volume_dir(): materializes ConfigMap/Secret keys as files on disk.
//! Uses real tempdirs (std::env::temp_dir + a random-ish suffix) since this
//! function does real filesystem I/O — no reason to fake that away when
//! it's this cheap to exercise for real.
use super::*;
use std::collections::BTreeMap;

fn tmp_dir(tag: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!(
        "nodelet-test-{tag}-{}-{:?}",
        std::process::id(),
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
    ));
    d
}

#[test]
fn writes_text_entries_as_files() {
    let dir = tmp_dir("text");
    let mut text = BTreeMap::new();
    text.insert("Corefile".to_string(), ".:53 {\n  forward . 8.8.8.8\n}\n".to_string());
    write_volume_dir(&dir, Some(text), None).unwrap();
    let content = std::fs::read_to_string(dir.join("Corefile")).unwrap();
    assert!(content.contains("forward"));
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn writes_binary_entries_as_files() {
    let dir = tmp_dir("binary");
    let mut binary = BTreeMap::new();
    binary.insert("cert.der".to_string(), vec![0u8, 1, 2, 255]);
    write_volume_dir(&dir, None, Some(binary)).unwrap();
    let content = std::fs::read(dir.join("cert.der")).unwrap();
    assert_eq!(content, vec![0u8, 1, 2, 255]);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn writes_both_text_and_binary_entries_together() {
    let dir = tmp_dir("both");
    let mut text = BTreeMap::new();
    text.insert("a.txt".to_string(), "hello".to_string());
    let mut binary = BTreeMap::new();
    binary.insert("b.bin".to_string(), vec![9u8, 9, 9]);
    write_volume_dir(&dir, Some(text), Some(binary)).unwrap();
    assert_eq!(std::fs::read_to_string(dir.join("a.txt")).unwrap(), "hello");
    assert_eq!(std::fs::read(dir.join("b.bin")).unwrap(), vec![9u8, 9, 9]);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn empty_configmap_still_creates_the_directory() {
    // A ConfigMap with no data keys is legal — the mount target directory
    // must still exist (an empty dir, not a missing one) or the container
    // fails to start on a directory-not-found error instead of just
    // seeing an empty directory.
    let dir = tmp_dir("empty");
    write_volume_dir(&dir, None, None).unwrap();
    assert!(dir.is_dir());
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn multiple_keys_all_get_written() {
    let dir = tmp_dir("multi");
    let mut text = BTreeMap::new();
    text.insert("one".to_string(), "1".to_string());
    text.insert("two".to_string(), "2".to_string());
    text.insert("three".to_string(), "3".to_string());
    write_volume_dir(&dir, Some(text), None).unwrap();
    assert_eq!(std::fs::read_to_string(dir.join("one")).unwrap(), "1");
    assert_eq!(std::fs::read_to_string(dir.join("two")).unwrap(), "2");
    assert_eq!(std::fs::read_to_string(dir.join("three")).unwrap(), "3");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn rewriting_an_existing_key_overwrites_the_file() {
    let dir = tmp_dir("overwrite");
    let mut first = BTreeMap::new();
    first.insert("k".to_string(), "old".to_string());
    write_volume_dir(&dir, Some(first), None).unwrap();
    let mut second = BTreeMap::new();
    second.insert("k".to_string(), "new".to_string());
    write_volume_dir(&dir, Some(second), None).unwrap();
    assert_eq!(std::fs::read_to_string(dir.join("k")).unwrap(), "new");
    std::fs::remove_dir_all(&dir).ok();
}
