//! rotate_log_file(): real filesystem renames against a temp dir — before
//! this, container logs grew forever with no rotation at all.
use super::*;

fn tmp(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("nodelet-test-logrotate-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir.join("app_0.log")
}

#[test]
fn rotating_renames_the_active_log_to_dot_one() {
    let log = tmp("basic");
    std::fs::write(&log, b"old content").unwrap();
    rotate_log_file(&log, 5).unwrap();
    assert!(!log.exists(), "the active path must be gone after rotation — the caller recreates it");
    let rotated = format!("{}.1", log.display());
    assert_eq!(std::fs::read_to_string(&rotated).unwrap(), "old content");
    std::fs::remove_dir_all(log.parent().unwrap()).unwrap();
}

#[test]
fn existing_rotated_files_shift_up_by_one() {
    let log = tmp("shift");
    std::fs::write(&log, b"newest").unwrap();
    std::fs::write(format!("{}.1", log.display()), b"was-1").unwrap();
    std::fs::write(format!("{}.2", log.display()), b"was-2").unwrap();

    rotate_log_file(&log, 5).unwrap();

    assert_eq!(std::fs::read_to_string(format!("{}.1", log.display())).unwrap(), "newest");
    assert_eq!(std::fs::read_to_string(format!("{}.2", log.display())).unwrap(), "was-1");
    assert_eq!(std::fs::read_to_string(format!("{}.3", log.display())).unwrap(), "was-2");
    std::fs::remove_dir_all(log.parent().unwrap()).unwrap();
}

#[test]
fn oldest_file_past_max_files_is_dropped() {
    let log = tmp("drop-oldest");
    std::fs::write(&log, b"newest").unwrap();
    std::fs::write(format!("{}.1", log.display()), b"was-1").unwrap();
    std::fs::write(format!("{}.2", log.display()), b"was-2").unwrap(); // max_files=3 -> this must be dropped

    rotate_log_file(&log, 3).unwrap();

    assert_eq!(std::fs::read_to_string(format!("{}.1", log.display())).unwrap(), "newest");
    assert_eq!(std::fs::read_to_string(format!("{}.2", log.display())).unwrap(), "was-1");
    assert!(!std::path::Path::new(&format!("{}.3", log.display())).exists(), "was-2 must be dropped, not renamed to .3");
    std::fs::remove_dir_all(log.parent().unwrap()).unwrap();
}

#[test]
fn max_files_of_one_keeps_no_rotated_copies_at_all() {
    let log = tmp("max-one");
    std::fs::write(&log, b"content").unwrap();
    rotate_log_file(&log, 1).unwrap();
    assert!(!log.exists());
    assert!(!std::path::Path::new(&format!("{}.1", log.display())).exists());
    std::fs::remove_dir_all(log.parent().unwrap()).unwrap();
}

#[test]
fn zero_max_files_is_clamped_to_one() {
    let log = tmp("zero");
    std::fs::write(&log, b"content").unwrap();
    rotate_log_file(&log, 0).unwrap(); // must not panic or divide oddly
    assert!(!log.exists());
    std::fs::remove_dir_all(log.parent().unwrap()).unwrap();
}
