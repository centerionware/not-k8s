use super::*;

#[test]
fn starts_at_the_given_initial_state() {
    assert!(ProbeTracker::new(true).passing);
    assert!(!ProbeTracker::new(false).passing);
}

#[test]
fn flips_to_failing_only_after_consecutive_failure_threshold() {
    let mut t = ProbeTracker::new(true);
    t.record(false, 1, 3);
    assert!(t.passing, "one failure short of threshold=3 must not flip yet");
    t.record(false, 1, 3);
    assert!(t.passing, "two failures short of threshold=3 must not flip yet");
    t.record(false, 1, 3);
    assert!(!t.passing, "third consecutive failure must flip to failing");
}

#[test]
fn a_single_success_resets_the_failure_streak() {
    let mut t = ProbeTracker::new(true);
    t.record(false, 1, 3);
    t.record(false, 1, 3);
    t.record(true, 1, 3); // resets failure streak
    t.record(false, 1, 3);
    t.record(false, 1, 3);
    assert!(t.passing, "streak was reset by the intervening success; two failures is not three");
}

#[test]
fn flips_to_passing_only_after_consecutive_success_threshold() {
    let mut t = ProbeTracker::new(false);
    t.record(true, 2, 3);
    assert!(!t.passing, "one success short of threshold=2 must not flip yet");
    t.record(true, 2, 3);
    assert!(t.passing, "second consecutive success must flip to passing");
}

#[test]
fn default_success_threshold_of_one_flips_immediately() {
    let mut t = ProbeTracker::new(false);
    t.record(true, 1, 3);
    assert!(t.passing);
}

#[test]
fn zero_thresholds_are_clamped_to_one() {
    let mut t = ProbeTracker::new(true);
    t.record(false, 0, 0);
    assert!(!t.passing, "threshold 0 must behave like threshold 1, not never-flip");
}
