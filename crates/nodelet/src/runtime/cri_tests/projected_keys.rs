//! write_projected_keys(): the merge-into-one-directory logic for a
//! projected volume's configMap/secret sources, plus the `items`
//! (KeyToPath) key-selection-and-rename Kubernetes supports here.
use super::*;
use k8s_openapi::api::core::v1::KeyToPath;
use std::collections::BTreeMap;

fn text_map(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
}

fn tmp(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("nodelet-test-projected-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

#[test]
fn no_items_writes_every_key_as_its_own_filename() {
    let dir = tmp("no-items");
    write_projected_keys(&dir, Some(text_map(&[("a", "1"), ("b", "2")])), None, None).unwrap();
    assert_eq!(std::fs::read_to_string(dir.join("a")).unwrap(), "1");
    assert_eq!(std::fs::read_to_string(dir.join("b")).unwrap(), "2");
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn items_select_and_rename_specific_keys() {
    let dir = tmp("items");
    let items = vec![KeyToPath { key: "a".to_string(), path: "renamed-a".to_string(), mode: None }];
    write_projected_keys(&dir, Some(text_map(&[("a", "1"), ("b", "2")])), None, Some(&items)).unwrap();
    assert_eq!(std::fs::read_to_string(dir.join("renamed-a")).unwrap(), "1");
    assert!(!dir.join("b").exists(), "keys not listed in items must not be written");
    assert!(!dir.join("a").exists(), "the original key name must not appear when items renames it");
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn items_path_with_subdirectory_creates_it() {
    let dir = tmp("items-nested");
    let items = vec![KeyToPath { key: "a".to_string(), path: "nested/a".to_string(), mode: None }];
    write_projected_keys(&dir, Some(text_map(&[("a", "1")])), None, Some(&items)).unwrap();
    assert_eq!(std::fs::read_to_string(dir.join("nested").join("a")).unwrap(), "1");
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn binary_data_is_written_too() {
    let dir = tmp("binary");
    let mut binary = std::collections::BTreeMap::new();
    binary.insert("blob".to_string(), vec![0u8, 1, 2, 255]);
    write_projected_keys(&dir, None, Some(binary), None).unwrap();
    assert_eq!(std::fs::read(dir.join("blob")).unwrap(), vec![0u8, 1, 2, 255]);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn multiple_sources_merge_into_the_same_directory() {
    // Simulates two calls (configMap source, then secret source) against
    // the same projected volume dir — real Kubernetes semantics.
    let dir = tmp("merge");
    write_projected_keys(&dir, Some(text_map(&[("from-cm", "x")])), None, None).unwrap();
    write_projected_keys(&dir, Some(text_map(&[("from-secret", "y")])), None, None).unwrap();
    assert_eq!(std::fs::read_to_string(dir.join("from-cm")).unwrap(), "x");
    assert_eq!(std::fs::read_to_string(dir.join("from-secret")).unwrap(), "y");
    std::fs::remove_dir_all(&dir).unwrap();
}
