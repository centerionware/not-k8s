//! recursive_read_only_cri(): volumeMounts[].recursiveReadOnly (round
//! 85; GA 1.33, KEP-3116; found in round 83's re-audit) -> CRI's plain
//! boolean Mount.recursive_read_only.
use super::*;

const PRIVATE: MountPropagation = MountPropagation::PropagationPrivate;
const HOST_TO_CONTAINER: MountPropagation = MountPropagation::PropagationHostToContainer;

#[test]
fn enabled_with_readonly_and_private_propagation_is_true() {
    assert!(recursive_read_only_cri(Some("Enabled"), true, PRIVATE));
}

#[test]
fn if_possible_with_readonly_and_private_propagation_is_true() {
    // Documented scope simplification: IfPossible is currently treated
    // identically to Enabled.
    assert!(recursive_read_only_cri(Some("IfPossible"), true, PRIVATE));
}

#[test]
fn disabled_is_always_false() {
    assert!(!recursive_read_only_cri(Some("Disabled"), true, PRIVATE));
}

#[test]
fn none_is_always_false() {
    assert!(!recursive_read_only_cri(None, true, PRIVATE));
}

#[test]
fn an_unrecognized_value_is_false() {
    assert!(!recursive_read_only_cri(Some("Bogus"), true, PRIVATE));
}

#[test]
fn enabled_without_readonly_is_false_per_the_cri_contract() {
    // The CRI proto's own contract: recursive_read_only: true requires
    // readonly: true. Never send a contract-violating combination.
    assert!(!recursive_read_only_cri(Some("Enabled"), false, PRIVATE));
}

#[test]
fn enabled_with_non_private_propagation_is_false_per_the_cri_contract() {
    assert!(!recursive_read_only_cri(Some("Enabled"), true, HOST_TO_CONTAINER));
}
