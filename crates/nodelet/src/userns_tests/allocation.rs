//! UsernsAllocator::allocate()/release(): exclusive per-pod host UID/GID
//! range assignment behind spec.hostUsers: false.
use super::*;

#[test]
fn first_allocation_gets_the_base_range() {
    let a = UsernsAllocator::new(100_000, 65_536, 4);
    assert_eq!(a.allocate("pod-1"), Some((100_000, 65_536)));
}

#[test]
fn a_second_pod_gets_a_disjoint_range() {
    let a = UsernsAllocator::new(100_000, 65_536, 4);
    let (base1, len1) = a.allocate("pod-1").unwrap();
    let (base2, len2) = a.allocate("pod-2").unwrap();
    assert_ne!(base1, base2);
    // Ranges must not overlap at all.
    assert!(base1 + len1 <= base2 || base2 + len2 <= base1);
}

#[test]
fn allocating_the_same_key_twice_returns_the_same_range() {
    let a = UsernsAllocator::new(100_000, 65_536, 4);
    let first = a.allocate("pod-1");
    let second = a.allocate("pod-1");
    assert_eq!(first, second);
    assert_eq!(a.claimed_count(), 1);
}

#[test]
fn release_frees_the_slot_for_reuse() {
    let a = UsernsAllocator::new(100_000, 65_536, 1);
    let first = a.allocate("pod-1").unwrap();
    assert!(a.allocate("pod-2").is_none()); // pool exhausted (max_slots=1)
    a.release("pod-1");
    let reused = a.allocate("pod-2").unwrap();
    assert_eq!(reused, first); // the only slot, now free again
}

#[test]
fn exhausted_pool_returns_none_not_a_panic() {
    let a = UsernsAllocator::new(100_000, 65_536, 2);
    assert!(a.allocate("pod-1").is_some());
    assert!(a.allocate("pod-2").is_some());
    assert!(a.allocate("pod-3").is_none());
}

#[test]
fn releasing_an_unknown_key_is_a_harmless_no_op() {
    let a = UsernsAllocator::new(100_000, 65_536, 4);
    a.release("never-allocated");
    assert_eq!(a.claimed_count(), 0);
}

#[test]
fn released_and_reallocated_slots_reuse_the_lowest_free_index() {
    let a = UsernsAllocator::new(100_000, 65_536, 3);
    a.allocate("pod-1"); // slot 0
    a.allocate("pod-2"); // slot 1
    a.allocate("pod-3"); // slot 2
    a.release("pod-2"); // slot 1 free again
    let (base, _) = a.allocate("pod-4").unwrap();
    assert_eq!(base, 100_000 + 65_536); // slot 1's base, not a new slot 3
}
