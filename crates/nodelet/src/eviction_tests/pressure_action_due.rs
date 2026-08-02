use super::*;
use std::time::Duration;

#[test]
fn hard_true_acts_immediately_regardless_of_soft_state() {
    assert!(pressure_action_due(true, None, Duration::from_secs(90)));
    assert!(pressure_action_due(true, Some(Duration::from_secs(0)), Duration::from_secs(90)));
}

#[test]
fn soft_true_but_not_yet_past_grace_period_does_not_act() {
    assert!(!pressure_action_due(false, Some(Duration::from_secs(89)), Duration::from_secs(90)));
}

#[test]
fn soft_true_past_grace_period_acts() {
    assert!(pressure_action_due(false, Some(Duration::from_secs(90)), Duration::from_secs(90)));
    assert!(pressure_action_due(false, Some(Duration::from_secs(200)), Duration::from_secs(90)));
}

#[test]
fn neither_hard_nor_soft_does_not_act() {
    assert!(!pressure_action_due(false, None, Duration::from_secs(90)));
}
