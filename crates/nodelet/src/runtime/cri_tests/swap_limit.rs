//! container_swap_limit_bytes(): the pure formula behind
//! memorySwap.swapBehavior (round 68; GA 1.34, found in round 65's
//! fresh gap re-audit) — NoSwap pins every limited container's swap
//! ceiling to its own memory limit (zero additional swap); LimitedSwap
//! implements KEP-2400's proportional-share formula, restricted in
//! effect to Burstable-shaped containers.
use super::*;

const GIB: i64 = 1024 * 1024 * 1024;

// --- NoSwap (memory_swap_limited: false) ---

#[test]
fn no_swap_pins_the_ceiling_to_the_memory_limit_when_one_is_set() {
    assert_eq!(container_swap_limit_bytes(1 * GIB, 2 * GIB, 8 * GIB, 4 * GIB, false), 2 * GIB);
}

#[test]
fn no_swap_with_no_memory_limit_leaves_it_unspecified() {
    assert_eq!(container_swap_limit_bytes(1 * GIB, 0, 8 * GIB, 4 * GIB, false), 0);
}

#[test]
fn no_swap_ignores_the_node_swap_capacity_entirely() {
    // Whatever node_swap_bytes is, NoSwap never grants anything beyond
    // the memory limit itself.
    assert_eq!(container_swap_limit_bytes(1 * GIB, 2 * GIB, 8 * GIB, 1000 * GIB, false), 2 * GIB);
}

// --- LimitedSwap: Guaranteed-shaped (request == limit) gets zero share ---

#[test]
fn limited_swap_guaranteed_shaped_container_gets_no_extra_swap() {
    assert_eq!(container_swap_limit_bytes(2 * GIB, 2 * GIB, 8 * GIB, 4 * GIB, true), 2 * GIB);
}

// --- LimitedSwap: BestEffort-shaped (no request) gets zero share ---

#[test]
fn limited_swap_besteffort_shaped_container_gets_no_extra_swap() {
    assert_eq!(container_swap_limit_bytes(0, 0, 8 * GIB, 4 * GIB, true), 0);
}

#[test]
fn limited_swap_besteffort_shaped_with_a_limit_still_gets_no_extra_swap() {
    // No request, but somehow has a limit (unusual, but the formula
    // should still zero the share since ContainerMemoryProportion is
    // defined off the request).
    assert_eq!(container_swap_limit_bytes(0, 2 * GIB, 8 * GIB, 4 * GIB, true), 2 * GIB);
}

// --- LimitedSwap: Burstable-shaped gets a proportional share ---

#[test]
fn limited_swap_burstable_shaped_container_gets_a_proportional_share() {
    // request=1GiB, node=8GiB memory, node swap=4GiB -> proportion=1/8,
    // swap_share = 4GiB/8 = 512MiB. Combined with the 2GiB limit.
    let result = container_swap_limit_bytes(1 * GIB, 2 * GIB, 8 * GIB, 4 * GIB, true);
    assert_eq!(result, 2 * GIB + 512 * 1024 * 1024);
}

#[test]
fn limited_swap_burstable_shaped_with_no_memory_limit_grants_nothing() {
    // No bound to combine a swap share with — documented scope limitation.
    assert_eq!(container_swap_limit_bytes(1 * GIB, 0, 8 * GIB, 4 * GIB, true), 0);
}

#[test]
fn limited_swap_swap_share_is_clamped_to_the_nodes_total_swap() {
    // request equals the whole node's memory -> proportion=1, would
    // otherwise ask for all of node_swap_bytes, which is exactly the
    // clamp ceiling (not exceeding it), plus the container's own limit.
    let result = container_swap_limit_bytes(8 * GIB, 8 * GIB - 1, 8 * GIB, 4 * GIB, true);
    // request != limit (off by one) so this is still Burstable-shaped.
    assert!(result >= 8 * GIB - 1);
    assert!(result <= (8 * GIB - 1) + 4 * GIB);
}

#[test]
fn limited_swap_zero_node_memory_falls_back_to_no_extra_swap_not_a_divide_by_zero_panic() {
    assert_eq!(container_swap_limit_bytes(1 * GIB, 2 * GIB, 0, 4 * GIB, true), 2 * GIB);
}

#[test]
fn limited_swap_zero_node_swap_grants_no_extra_share_even_for_burstable() {
    assert_eq!(container_swap_limit_bytes(1 * GIB, 2 * GIB, 8 * GIB, 0, true), 2 * GIB);
}
