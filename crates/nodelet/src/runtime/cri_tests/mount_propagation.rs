//! mount_propagation_cri(): volumeMounts[].mountPropagation (round 84;
//! found in round 83's re-audit) -> CRI's MountPropagation enum.
use super::*;

#[test]
fn none_defaults_to_private() {
    assert_eq!(mount_propagation_cri(None), MountPropagation::PropagationPrivate);
}

#[test]
fn host_to_container_maps_correctly() {
    assert_eq!(mount_propagation_cri(Some("HostToContainer")), MountPropagation::PropagationHostToContainer);
}

#[test]
fn bidirectional_maps_correctly() {
    assert_eq!(mount_propagation_cri(Some("Bidirectional")), MountPropagation::PropagationBidirectional);
}

#[test]
fn explicit_none_string_maps_to_private() {
    assert_eq!(mount_propagation_cri(Some("None")), MountPropagation::PropagationPrivate);
}

#[test]
fn an_unrecognized_value_falls_back_to_private() {
    assert_eq!(mount_propagation_cri(Some("Bogus")), MountPropagation::PropagationPrivate);
}
