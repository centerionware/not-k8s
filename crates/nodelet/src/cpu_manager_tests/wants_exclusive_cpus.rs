use super::*;

#[test]
fn besteffort_never_wants_exclusive_cpus() {
    assert_eq!(wants_exclusive_cpus(QosClass::BestEffort, Some(2000)), None);
}

#[test]
fn burstable_never_wants_exclusive_cpus() {
    assert_eq!(wants_exclusive_cpus(QosClass::Burstable, Some(2000)), None);
}

#[test]
fn guaranteed_with_a_whole_cpu_count_wants_that_many() {
    assert_eq!(wants_exclusive_cpus(QosClass::Guaranteed, Some(2000)), Some(2));
    assert_eq!(wants_exclusive_cpus(QosClass::Guaranteed, Some(1000)), Some(1));
}

#[test]
fn guaranteed_with_a_fractional_cpu_count_does_not_want_exclusive_cpus() {
    // 500m Guaranteed is real and common (e.g. cpu: "500m" for both
    // request and limit) — it must fall back to the shared pool, not
    // panic or round up to a whole core it didn't ask for.
    assert_eq!(wants_exclusive_cpus(QosClass::Guaranteed, Some(1500)), None);
    assert_eq!(wants_exclusive_cpus(QosClass::Guaranteed, Some(500)), None);
}

#[test]
fn guaranteed_with_no_cpu_limit_at_all_does_not_want_exclusive_cpus() {
    assert_eq!(wants_exclusive_cpus(QosClass::Guaranteed, None), None);
}

#[test]
fn zero_or_negative_is_never_exclusive() {
    assert_eq!(wants_exclusive_cpus(QosClass::Guaranteed, Some(0)), None);
}
