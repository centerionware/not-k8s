//! read_termination_message()/termination_message_host_path(): the pure
//! (or nearly-pure, filesystem-touching) pieces behind terminationMessagePath
//! read-back (round 24) — build_status() reads this file back for every
//! exited container to populate ContainerStatus.state.terminated.message.
use super::*;

#[test]
fn host_path_is_scoped_by_pod_uid_and_container_name() {
    let path = termination_message_host_path("pod-uid-1", "app");
    assert!(path.to_string_lossy().contains("pod-uid-1"));
    assert!(path.to_string_lossy().contains("app"));
    assert!(path.to_string_lossy().contains("termination-log"));
}

#[test]
fn read_termination_message_is_empty_for_a_missing_file() {
    let path = std::path::Path::new("/nonexistent/definitely-not-here/termination-log");
    assert_eq!(read_termination_message(path), "");
}

#[test]
fn read_termination_message_reads_a_small_files_full_content() {
    let dir = std::env::temp_dir().join(format!("nodelet-test-{}-{}", std::process::id(), line!()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("small.log");
    std::fs::write(&path, "short failure reason").unwrap();
    assert_eq!(read_termination_message(&path), "short failure reason");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn read_termination_message_keeps_only_the_last_bytes_of_an_oversized_file() {
    let dir = std::env::temp_dir().join(format!("nodelet-test-oversized-{}-{}", std::process::id(), line!()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("big.log");
    let mut content = "x".repeat(5000);
    content.push_str("END-MARKER");
    std::fs::write(&path, &content).unwrap();
    let got = read_termination_message(&path);
    assert!(got.len() <= MAX_TERMINATION_MESSAGE_BYTES);
    assert!(got.ends_with("END-MARKER"));
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn read_termination_message_of_an_empty_file_is_an_empty_string() {
    let dir = std::env::temp_dir().join(format!("nodelet-test-empty-{}-{}", std::process::id(), line!()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("empty.log");
    std::fs::write(&path, "").unwrap();
    assert_eq!(read_termination_message(&path), "");
    std::fs::remove_dir_all(&dir).ok();
}
