use super::*;

#[test]
fn splits_total_between_non_critical_and_critical() {
    let (non_critical, critical) = budget_split(90, 30);
    assert_eq!(non_critical, 60);
    assert_eq!(critical, 30);
}

#[test]
fn zero_critical_budget_gives_everything_to_non_critical() {
    let (non_critical, critical) = budget_split(90, 0);
    assert_eq!(non_critical, 90);
    assert_eq!(critical, 0);
}

#[test]
fn critical_budget_larger_than_total_is_clamped_not_negative() {
    // Config::from_env() already clamps this at load time, but the pure
    // function shouldn't silently underflow (u64) if it's ever called with
    // an inconsistent pair directly, e.g. from a future test or caller.
    let (non_critical, critical) = budget_split(30, 90);
    assert_eq!(critical, 30);
    assert_eq!(non_critical, 0);
}

#[test]
fn zero_total_budget_gives_zero_to_both() {
    let (non_critical, critical) = budget_split(0, 0);
    assert_eq!(non_critical, 0);
    assert_eq!(critical, 0);
}
