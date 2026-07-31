use super::*;

#[test]
fn besteffort_never_wants_pinned_memory() {
    assert_eq!(wants_pinned_memory(QosClass::BestEffort, Some(1_000_000)), None);
}

#[test]
fn burstable_never_wants_pinned_memory() {
    assert_eq!(wants_pinned_memory(QosClass::Burstable, Some(1_000_000)), None);
}

#[test]
fn guaranteed_with_a_memory_limit_wants_pinning() {
    assert_eq!(wants_pinned_memory(QosClass::Guaranteed, Some(1_000_000)), Some(1_000_000));
}

#[test]
fn guaranteed_with_no_memory_limit_does_not_want_pinning() {
    assert_eq!(wants_pinned_memory(QosClass::Guaranteed, None), None);
}

#[test]
fn zero_or_negative_is_never_pinned() {
    assert_eq!(wants_pinned_memory(QosClass::Guaranteed, Some(0)), None);
    assert_eq!(wants_pinned_memory(QosClass::Guaranteed, Some(-1)), None);
}

#[test]
fn unlike_cpu_manager_fractional_or_odd_byte_counts_still_qualify() {
    // Memory has no "integer" requirement the way CPU Manager's whole-core
    // rule does — any positive limit is eligible.
    assert_eq!(wants_pinned_memory(QosClass::Guaranteed, Some(12345)), Some(12345));
}
