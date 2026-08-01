//! apply_fs_group(): real chown(2)/chmod(2) against a temp directory tree —
//! before this, `securityContext.fsGroup` had no effect on materialized
//! volumes at all. Uses the test process's own primary gid so this passes
//! without root (chown to an arbitrary group would EPERM unprivileged).
use super::*;

fn own_gid() -> u32 {
    unsafe { libc::getgid() }
}

fn tmp(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("nodelet-test-fsgroup-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn chown_gid_succeeds_against_the_callers_own_group() {
    let dir = tmp("chown");
    chown_gid(&dir, own_gid()).expect("chowning to one's own gid must succeed unprivileged");
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn set_setgid_sets_the_bit() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tmp("setgid");
    set_setgid(&dir).unwrap();
    let mode = std::fs::metadata(&dir).unwrap().permissions().mode();
    assert_eq!(mode & 0o2000, 0o2000);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn apply_fs_group_recurses_into_subdirectories_and_files() {
    let dir = tmp("recurse");
    std::fs::create_dir_all(dir.join("sub")).unwrap();
    std::fs::write(dir.join("file.txt"), b"hi").unwrap();
    std::fs::write(dir.join("sub").join("nested.txt"), b"hi").unwrap();

    apply_fs_group(&dir, own_gid()).expect("recursive fsGroup application must succeed unprivileged");

    use std::os::unix::fs::MetadataExt;
    assert_eq!(std::fs::metadata(&dir).unwrap().gid(), own_gid());
    assert_eq!(std::fs::metadata(dir.join("file.txt")).unwrap().gid(), own_gid());
    assert_eq!(std::fs::metadata(dir.join("sub")).unwrap().gid(), own_gid());
    assert_eq!(std::fs::metadata(dir.join("sub").join("nested.txt")).unwrap().gid(), own_gid());

    use std::os::unix::fs::PermissionsExt;
    let sub_mode = std::fs::metadata(dir.join("sub")).unwrap().permissions().mode();
    assert_eq!(sub_mode & 0o2000, 0o2000, "nested directories must get the setgid bit too");

    std::fs::remove_dir_all(&dir).unwrap();
}

/// `requires_fs_group_change()`/`skip_fs_group_change()` (round 93;
/// found in round 92's re-audit) — `fsGroupChangePolicy: OnRootMismatch`
/// for CSI/PV-backed volumes only, matching upstream's own
/// `requiresPermissionChange()`.
mod change_policy {
    use super::*;

    #[test]
    fn matching_gid_and_setgid_does_not_require_change() {
        assert!(!requires_fs_group_change(4321, true, 4321));
    }

    #[test]
    fn mismatched_gid_requires_change() {
        assert!(requires_fs_group_change(1000, true, 4321));
    }

    #[test]
    fn missing_setgid_bit_requires_change_even_with_matching_gid() {
        assert!(requires_fs_group_change(4321, false, 4321));
    }

    #[test]
    fn always_policy_never_skips() {
        let dir = tmp("policy-always");
        apply_fs_group(&dir, own_gid()).unwrap();
        assert!(!skip_fs_group_change(&dir, own_gid(), Some("Always")));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn unset_policy_never_skips_matching_the_apis_own_always_default() {
        let dir = tmp("policy-unset");
        apply_fs_group(&dir, own_gid()).unwrap();
        assert!(!skip_fs_group_change(&dir, own_gid(), None));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn on_root_mismatch_skips_when_already_applied() {
        let dir = tmp("policy-onrootmismatch-match");
        apply_fs_group(&dir, own_gid()).unwrap();
        assert!(skip_fs_group_change(&dir, own_gid(), Some("OnRootMismatch")));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn on_root_mismatch_does_not_skip_when_gid_differs() {
        let dir = tmp("policy-onrootmismatch-mismatch");
        // A fresh temp dir's gid won't already be own_gid()+1 (an
        // arbitrary different value never actually chowned to).
        assert!(!skip_fs_group_change(&dir, own_gid() + 1, Some("OnRootMismatch")));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_stat_failure_never_skips() {
        let missing = std::env::temp_dir().join("nodelet-test-fsgroup-does-not-exist");
        let _ = std::fs::remove_dir_all(&missing);
        assert!(!skip_fs_group_change(&missing, 4321, Some("OnRootMismatch")));
    }
}
