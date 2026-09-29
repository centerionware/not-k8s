use super::*;
use std::os::unix::net::UnixListener;
use std::sync::atomic::{AtomicU64, Ordering};

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn scratch_dir() -> PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("nodelet-plugin-registry-test-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn nonexistent_directory_returns_empty_not_a_panic() {
    let dir = std::env::temp_dir().join("nodelet-plugin-registry-test-does-not-exist");
    assert!(scan_registry_dir(&dir).is_empty());
}

#[test]
fn empty_directory_returns_empty() {
    let dir = scratch_dir();
    assert!(scan_registry_dir(&dir).is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn finds_a_real_unix_socket() {
    let dir = scratch_dir();
    let sock_path = dir.join("driver.sock");
    let _listener = UnixListener::bind(&sock_path).unwrap();

    let found = scan_registry_dir(&dir);
    assert_eq!(found, vec![sock_path]);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn ignores_regular_files() {
    let dir = scratch_dir();
    std::fs::write(dir.join("not-a-socket.txt"), b"hello").unwrap();

    assert!(scan_registry_dir(&dir).is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn ignores_subdirectories() {
    let dir = scratch_dir();
    std::fs::create_dir(dir.join("a-subdir")).unwrap();

    assert!(scan_registry_dir(&dir).is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn finds_multiple_sockets_and_ignores_a_mixed_in_regular_file() {
    let dir = scratch_dir();
    let sock_a = dir.join("a.sock");
    let sock_b = dir.join("b.sock");
    let _listener_a = UnixListener::bind(&sock_a).unwrap();
    let _listener_b = UnixListener::bind(&sock_b).unwrap();
    std::fs::write(dir.join("README"), b"not a socket").unwrap();

    let mut found = scan_registry_dir(&dir);
    found.sort();
    let mut expected = vec![sock_a, sock_b];
    expected.sort();
    assert_eq!(found, expected);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn default_registry_keeps_watching_the_source_kubelet_directory() {
    assert_eq!(
        registry_dirs(DEFAULT_REGISTRY_DIR),
        vec![
            PathBuf::from(DEFAULT_REGISTRY_DIR),
            PathBuf::from(KUBELET_REGISTRY_DIR)
        ]
    );
    assert_eq!(
        registry_dirs("/custom/plugins_registry"),
        vec![PathBuf::from("/custom/plugins_registry")]
    );
}

#[test]
fn finds_sockets_in_both_registry_directories() {
    let nodelet_dir = scratch_dir();
    let kubelet_dir = scratch_dir();
    let nodelet_socket = nodelet_dir.join("nodelet-driver.sock");
    let kubelet_socket = kubelet_dir.join("source-driver.sock");
    let _nodelet_listener = UnixListener::bind(&nodelet_socket).unwrap();
    let _kubelet_listener = UnixListener::bind(&kubelet_socket).unwrap();

    let mut found = scan_registry_dirs(&[nodelet_dir.clone(), kubelet_dir.clone()]);
    found.sort();
    let mut expected = vec![nodelet_socket, kubelet_socket];
    expected.sort();
    assert_eq!(found, expected);
    let _ = std::fs::remove_dir_all(nodelet_dir);
    let _ = std::fs::remove_dir_all(kubelet_dir);
}
