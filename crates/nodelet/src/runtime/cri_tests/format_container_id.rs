//! format_container_id(): real kubelet's <runtimeName>://<id> container-ID
//! format (Round 57; found in round 54's re-audit).
use super::*;

#[test]
fn combines_runtime_name_and_id_with_a_scheme_separator() {
    assert_eq!(format_container_id("containerd", "abc123"), "containerd://abc123");
}

#[test]
fn falls_back_runtime_name_still_produces_a_valid_looking_id() {
    assert_eq!(format_container_id("unknown", "abc123"), "unknown://abc123");
}
