//! is_hugepages_medium_empty_dir()/hugepages_medium_pagesize_option()/
//! hugetlbfs_mount_args(): the pure logic behind emptyDir.medium:
//! "HugePages"/"HugePages-<size>" (round 61, the last of round 58's 3
//! HugePages pieces) — whether a volume wants hugetlbfs at all, its
//! pagesize option, and the mount(8) invocation to give it one.
use super::*;
use k8s_openapi::api::core::v1::EmptyDirVolumeSource;
use std::path::Path;

fn empty_dir(medium: Option<&str>) -> EmptyDirVolumeSource {
    EmptyDirVolumeSource { medium: medium.map(str::to_string), ..Default::default() }
}

#[test]
fn no_medium_is_not_hugepages_backed() {
    assert!(!is_hugepages_medium_empty_dir(&empty_dir(None)));
}

#[test]
fn medium_memory_is_not_hugepages_backed() {
    assert!(!is_hugepages_medium_empty_dir(&empty_dir(Some("Memory"))));
}

#[test]
fn bare_hugepages_medium_is_hugepages_backed() {
    assert!(is_hugepages_medium_empty_dir(&empty_dir(Some("HugePages"))));
}

#[test]
fn sized_hugepages_medium_is_hugepages_backed() {
    assert!(is_hugepages_medium_empty_dir(&empty_dir(Some("HugePages-2Mi"))));
}

#[test]
fn medium_is_case_sensitive() {
    assert!(!is_hugepages_medium_empty_dir(&empty_dir(Some("hugepages-2Mi"))));
}

#[test]
fn bare_hugepages_has_no_pagesize_option() {
    assert_eq!(hugepages_medium_pagesize_option("HugePages"), None);
}

#[test]
fn sized_hugepages_converts_the_binary_suffix_to_a_bare_pagesize_option() {
    assert_eq!(hugepages_medium_pagesize_option("HugePages-2Mi"), Some("pagesize=2M".to_string()));
    assert_eq!(hugepages_medium_pagesize_option("HugePages-1Gi"), Some("pagesize=1G".to_string()));
    assert_eq!(hugepages_medium_pagesize_option("HugePages-64Ki"), Some("pagesize=64K".to_string()));
}

#[test]
fn bare_hugepages_mount_args_have_no_dash_o_at_all() {
    let args = hugetlbfs_mount_args(Path::new("/vol"), "HugePages", None);
    assert_eq!(args, vec!["-t", "hugetlbfs", "none", "/vol"]);
}

#[test]
fn sized_hugepages_mount_args_include_the_pagesize_option() {
    let args = hugetlbfs_mount_args(Path::new("/vol"), "HugePages-2Mi", None);
    assert_eq!(args, vec!["-t", "hugetlbfs", "-o", "pagesize=2M", "none", "/vol"]);
}

#[test]
fn a_size_limit_is_appended_to_the_same_dash_o_option() {
    let args = hugetlbfs_mount_args(Path::new("/vol"), "HugePages-2Mi", Some(4_194_304));
    assert_eq!(args, vec!["-t", "hugetlbfs", "-o", "pagesize=2M,size=4194304", "none", "/vol"]);
}

#[test]
fn a_size_limit_with_no_pagesize_still_gets_its_own_dash_o() {
    let args = hugetlbfs_mount_args(Path::new("/vol"), "HugePages", Some(4_194_304));
    assert_eq!(args, vec!["-t", "hugetlbfs", "-o", "size=4194304", "none", "/vol"]);
}

#[test]
fn a_zero_or_negative_size_limit_is_treated_as_unset() {
    assert_eq!(hugetlbfs_mount_args(Path::new("/vol"), "HugePages", Some(0)), vec!["-t", "hugetlbfs", "none", "/vol"]);
    assert_eq!(hugetlbfs_mount_args(Path::new("/vol"), "HugePages", Some(-1)), vec!["-t", "hugetlbfs", "none", "/vol"]);
}
