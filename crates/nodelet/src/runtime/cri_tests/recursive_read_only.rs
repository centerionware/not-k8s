//! recursive_read_only_cri(): volumeMounts[].recursiveReadOnly (round
//! 85; GA 1.33, KEP-3116; found in round 83's re-audit) -> CRI's plain
//! boolean Mount.recursive_read_only. Round 97 gave `IfPossible` a real
//! best-effort fallback, gated on `handler_supports_recursive_ro`.
use super::*;

const PRIVATE: MountPropagation = MountPropagation::PropagationPrivate;
const HOST_TO_CONTAINER: MountPropagation = MountPropagation::PropagationHostToContainer;

#[test]
fn enabled_with_readonly_and_private_propagation_is_true_regardless_of_handler_support() {
    assert!(recursive_read_only_cri(Some("Enabled"), true, PRIVATE, false));
    assert!(recursive_read_only_cri(Some("Enabled"), true, PRIVATE, true));
}

#[test]
fn if_possible_is_true_only_when_the_handler_supports_it() {
    assert!(recursive_read_only_cri(Some("IfPossible"), true, PRIVATE, true));
    assert!(!recursive_read_only_cri(Some("IfPossible"), true, PRIVATE, false), "IfPossible must fall back to a plain (non-recursive) read-only mount when the handler doesn't advertise support");
}

#[test]
fn disabled_is_always_false() {
    assert!(!recursive_read_only_cri(Some("Disabled"), true, PRIVATE, true));
}

#[test]
fn none_is_always_false() {
    assert!(!recursive_read_only_cri(None, true, PRIVATE, true));
}

#[test]
fn an_unrecognized_value_is_false() {
    assert!(!recursive_read_only_cri(Some("Bogus"), true, PRIVATE, true));
}

#[test]
fn enabled_without_readonly_is_false_per_the_cri_contract() {
    // The CRI proto's own contract: recursive_read_only: true requires
    // readonly: true. Never send a contract-violating combination.
    assert!(!recursive_read_only_cri(Some("Enabled"), false, PRIVATE, true));
}

#[test]
fn enabled_with_non_private_propagation_is_false_per_the_cri_contract() {
    assert!(!recursive_read_only_cri(Some("Enabled"), true, HOST_TO_CONTAINER, true));
}
