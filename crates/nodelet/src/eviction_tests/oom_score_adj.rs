//! oom_score_adj(): real kubelet's per-container OOM score adjustment
//! formula (round 28) — Guaranteed/BestEffort fixed values, Burstable
//! scaled by the container's own memory request against node capacity.
use super::*;

const GIB: i64 = 1024 * 1024 * 1024;

#[test]
fn guaranteed_is_always_the_fixed_protected_value() {
    assert_eq!(oom_score_adj(QosClass::Guaranteed, 0, 4 * GIB), -998);
    assert_eq!(oom_score_adj(QosClass::Guaranteed, 4 * GIB, 4 * GIB), -998);
}

#[test]
fn besteffort_is_always_the_fixed_certain_death_value() {
    assert_eq!(oom_score_adj(QosClass::BestEffort, 0, 4 * GIB), 1000);
    assert_eq!(oom_score_adj(QosClass::BestEffort, 1, 4 * GIB), 1000);
}

#[test]
fn burstable_with_no_request_gets_the_maximum_burstable_score() {
    // 1000 - (1000 * 0 / capacity) = 1000, then clamped to 999 (must
    // never reach BestEffort's exact 1000).
    assert_eq!(oom_score_adj(QosClass::Burstable, 0, 4 * GIB), 999);
}

#[test]
fn burstable_scales_down_as_the_requested_share_grows() {
    // Requesting half the node's memory: 1000 - (1000 * 0.5) = 500.
    let half = 2 * GIB;
    assert_eq!(oom_score_adj(QosClass::Burstable, half, 4 * GIB), 500);
}

#[test]
fn burstable_requesting_the_entire_node_clamps_to_the_floor() {
    // 1000 - (1000 * capacity / capacity) = 0, clamped up to 2.
    assert_eq!(oom_score_adj(QosClass::Burstable, 4 * GIB, 4 * GIB), 2);
}

#[test]
fn burstable_requesting_more_than_capacity_still_clamps_to_the_floor_not_negative() {
    assert_eq!(oom_score_adj(QosClass::Burstable, 8 * GIB, 4 * GIB), 2);
}

#[test]
fn degenerate_zero_node_capacity_falls_back_to_999_not_a_divide_by_zero_panic() {
    assert_eq!(oom_score_adj(QosClass::Burstable, GIB, 0), 999);
}

#[test]
fn negative_node_capacity_also_falls_back_to_999() {
    assert_eq!(oom_score_adj(QosClass::Burstable, GIB, -1), 999);
}
