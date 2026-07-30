use super::*;

fn tmp(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("nodelet-test-staticpods-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn lists_regular_files_sorted() {
    let dir = tmp("sorted");
    std::fs::write(dir.join("b.yaml"), b"").unwrap();
    std::fs::write(dir.join("a.yaml"), b"").unwrap();
    let files = scan_manifest_dir(&dir);
    assert_eq!(files, vec![dir.join("a.yaml"), dir.join("b.yaml")]);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn dotfiles_are_skipped() {
    let dir = tmp("dotfiles");
    std::fs::write(dir.join(".hidden.yaml"), b"").unwrap();
    std::fs::write(dir.join("visible.yaml"), b"").unwrap();
    let files = scan_manifest_dir(&dir);
    assert_eq!(files, vec![dir.join("visible.yaml")]);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn subdirectories_are_skipped_not_recursed_into() {
    let dir = tmp("subdir");
    std::fs::create_dir(dir.join("nested")).unwrap();
    std::fs::write(dir.join("nested").join("pod.yaml"), b"").unwrap();
    std::fs::write(dir.join("top.yaml"), b"").unwrap();
    let files = scan_manifest_dir(&dir);
    assert_eq!(files, vec![dir.join("top.yaml")]);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn nonexistent_directory_returns_empty_not_an_error() {
    assert!(scan_manifest_dir(Path::new("/this/does/not/exist/hopefully")).is_empty());
}

#[test]
fn empty_directory_returns_empty() {
    let dir = tmp("empty");
    assert!(scan_manifest_dir(&dir).is_empty());
    std::fs::remove_dir_all(&dir).unwrap();
}
