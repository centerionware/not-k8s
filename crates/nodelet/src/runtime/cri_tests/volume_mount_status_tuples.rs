//! volume_mount_status_tuples(): containerStatuses[].volumeMounts
//! (round 91; found in round 89's re-audit) -- the reporting half of
//! round 85's recursive_read_only_cri() (round 97's real IfPossible
//! fallback applies here too).
use super::*;

fn vm(name: &str, mount_path: &str) -> k8s_openapi::api::core::v1::VolumeMount {
    k8s_openapi::api::core::v1::VolumeMount { name: name.to_string(), mount_path: mount_path.to_string(), ..Default::default() }
}

#[test]
fn empty_input_is_empty_output() {
    assert!(volume_mount_status_tuples(&[], false).is_empty());
}

#[test]
fn non_readonly_mount_has_no_recursive_read_only_field() {
    let mounts = volume_mount_status_tuples(&[vm("shared", "/shared")], false);
    assert_eq!(mounts, vec![("shared".to_string(), "/shared".to_string(), false, None)]);
}

#[test]
fn readonly_enabled_reports_enabled() {
    let mut m = vm("cfg", "/cfg");
    m.read_only = Some(true);
    m.recursive_read_only = Some("Enabled".to_string());
    let mounts = volume_mount_status_tuples(&[m], false);
    assert_eq!(mounts, vec![("cfg".to_string(), "/cfg".to_string(), true, Some("Enabled".to_string()))]);
}

#[test]
fn readonly_if_possible_reports_enabled_only_when_the_handler_supports_it() {
    // Round 97: IfPossible is a real best-effort fallback now, gated on
    // handler_supports_recursive_ro.
    let mut m = vm("cfg", "/cfg");
    m.read_only = Some(true);
    m.recursive_read_only = Some("IfPossible".to_string());
    assert_eq!(volume_mount_status_tuples(&[m.clone()], true)[0].3, Some("Enabled".to_string()));
    assert_eq!(volume_mount_status_tuples(&[m], false)[0].3, Some("Disabled".to_string()));
}

#[test]
fn readonly_without_recursive_request_reports_disabled() {
    let mut m = vm("cfg", "/cfg");
    m.read_only = Some(true);
    let mounts = volume_mount_status_tuples(&[m], true);
    assert_eq!(mounts[0].3, Some("Disabled".to_string()));
}

#[test]
fn readonly_enabled_with_non_private_propagation_reports_disabled() {
    let mut m = vm("cfg", "/cfg");
    m.read_only = Some(true);
    m.recursive_read_only = Some("Enabled".to_string());
    m.mount_propagation = Some("Bidirectional".to_string());
    let mounts = volume_mount_status_tuples(&[m], true);
    assert_eq!(mounts[0].3, Some("Disabled".to_string()));
}

#[test]
fn multiple_mounts_preserve_order() {
    let mounts = volume_mount_status_tuples(&[vm("a", "/a"), vm("b", "/b")], false);
    assert_eq!(mounts.iter().map(|m| m.0.as_str()).collect::<Vec<_>>(), vec!["a", "b"]);
}
