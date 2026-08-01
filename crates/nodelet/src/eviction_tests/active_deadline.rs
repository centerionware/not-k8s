//! active_deadline_exceeded() (round 81; found in round 80's re-audit):
//! spec.activeDeadlineSeconds -- real kubelet's own job, independent of
//! both eviction and restartPolicy.
use super::*;

#[test]
fn elapsed_past_the_deadline_exceeds() {
    assert!(active_deadline_exceeded(Some(60), Some(61)));
}

#[test]
fn elapsed_exactly_at_the_deadline_exceeds() {
    assert!(active_deadline_exceeded(Some(60), Some(60)));
}

#[test]
fn elapsed_under_the_deadline_does_not_exceed() {
    assert!(!active_deadline_exceeded(Some(60), Some(59)));
}

#[test]
fn no_deadline_configured_never_exceeds() {
    assert!(!active_deadline_exceeded(None, Some(1_000_000)));
}

#[test]
fn no_recorded_start_time_never_exceeds() {
    // A pod that hasn't recorded status.startTime yet -- never treated
    // as "already exceeded" from missing data.
    assert!(!active_deadline_exceeded(Some(60), None));
}

#[test]
fn neither_set_never_exceeds() {
    assert!(!active_deadline_exceeded(None, None));
}

#[test]
fn a_zero_deadline_is_exceeded_immediately() {
    assert!(active_deadline_exceeded(Some(0), Some(0)));
}
