//! is_memory_medium_empty_dir()/tmpfs_mount_args(): the pure logic behind
//! emptyDir.medium: Memory (round 30) — whether a volume wants tmpfs at
//! all, and the mount(8) invocation to give it one.
use super::*;
use k8s_openapi::api::core::v1::EmptyDirVolumeSource;
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use std::path::Path;

fn empty_dir(medium: Option<&str>) -> EmptyDirVolumeSource {
    EmptyDirVolumeSource { medium: medium.map(str::to_string), ..Default::default() }
}

#[test]
fn no_medium_is_not_memory_backed() {
    assert!(!is_memory_medium_empty_dir(&empty_dir(None)));
}

#[test]
fn empty_string_medium_is_not_memory_backed() {
    // The default medium is represented as an unset field, but the API
    // schema also technically allows an explicit empty string for it.
    assert!(!is_memory_medium_empty_dir(&empty_dir(Some(""))));
}

#[test]
fn medium_memory_is_memory_backed() {
    assert!(is_memory_medium_empty_dir(&empty_dir(Some("Memory"))));
}

#[test]
fn medium_is_case_sensitive() {
    assert!(!is_memory_medium_empty_dir(&empty_dir(Some("memory"))));
}

#[test]
fn no_size_limit_omits_the_dash_o_size_option() {
    let args = tmpfs_mount_args(Path::new("/var/lib/nodelet/pods/u/volumes/v"), None);
    assert_eq!(args, vec!["-t", "tmpfs", "tmpfs", "/var/lib/nodelet/pods/u/volumes/v"]);
}

#[test]
fn a_size_limit_adds_the_dash_o_size_option() {
    let args = tmpfs_mount_args(Path::new("/vol"), Some(67_108_864));
    assert_eq!(args, vec!["-t", "tmpfs", "-o", "size=67108864", "tmpfs", "/vol"]);
}

#[test]
fn a_zero_or_negative_size_limit_is_treated_as_unset() {
    assert_eq!(tmpfs_mount_args(Path::new("/vol"), Some(0)), vec!["-t", "tmpfs", "tmpfs", "/vol"]);
    assert_eq!(tmpfs_mount_args(Path::new("/vol"), Some(-1)), vec!["-t", "tmpfs", "tmpfs", "/vol"]);
}

#[test]
fn parse_memory_bytes_feeds_the_size_limit_correctly() {
    // Sanity-check the actual glue: a Quantity from the API round-trips
    // into the byte value tmpfs_mount_args() expects.
    let bytes = parse_memory_bytes(&Quantity("64Mi".to_string()));
    assert_eq!(bytes, Some(64 * 1024 * 1024));
}
