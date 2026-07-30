use super::*;

fn tmp_manifest(name: &str, pod_name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("nodelet-test-loadchanged-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("pod.yaml");
    std::fs::write(
        &path,
        format!(
            "apiVersion: v1\nkind: Pod\nmetadata:\n  name: {pod_name}\nspec:\n  containers:\n    - name: app\n      image: busybox\n"
        ),
    )
    .unwrap();
    path
}

#[test]
fn first_load_with_no_previous_hash_always_returns_the_parsed_pod() {
    let path = tmp_manifest("first", "static-a");
    let (hash, pod) = load_if_changed(&path, None, "node-1").unwrap().expect("first load must not be skipped");
    assert_eq!(pod.metadata.name.as_deref(), Some("static-a"));
    assert_ne!(hash, 0);
    std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
}

#[test]
fn unchanged_content_with_matching_hash_is_skipped() {
    let path = tmp_manifest("unchanged", "static-b");
    let (hash, _) = load_if_changed(&path, None, "node-1").unwrap().unwrap();
    let result = load_if_changed(&path, Some(hash), "node-1").unwrap();
    assert!(result.is_none(), "an unchanged manifest must be skipped, not re-parsed and re-applied");
    std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
}

#[test]
fn changed_content_with_a_different_hash_is_reloaded() {
    let path = tmp_manifest("changed", "static-c");
    let (hash, _) = load_if_changed(&path, None, "node-1").unwrap().unwrap();
    std::fs::write(&path, "apiVersion: v1\nkind: Pod\nmetadata:\n  name: static-c-v2\nspec:\n  containers:\n    - name: app\n      image: busybox:v2\n").unwrap();
    let (new_hash, pod) = load_if_changed(&path, Some(hash), "node-1").unwrap().expect("changed content must reload");
    assert_ne!(new_hash, hash);
    assert_eq!(pod.metadata.name.as_deref(), Some("static-c-v2"));
    std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
}

#[test]
fn unreadable_path_returns_an_error() {
    let result = load_if_changed(Path::new("/this/does/not/exist/hopefully.yaml"), None, "node-1");
    assert!(result.is_err());
}
