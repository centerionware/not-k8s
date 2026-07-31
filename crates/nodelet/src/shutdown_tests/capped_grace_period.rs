use super::*;

#[test]
fn pod_grace_period_within_budget_is_unchanged() {
    assert_eq!(capped_grace_period(Some(10), 60), 10);
}

#[test]
fn pod_grace_period_exceeding_budget_is_capped() {
    assert_eq!(capped_grace_period(Some(300), 30), 30);
}

#[test]
fn unset_pod_grace_period_defaults_to_thirty_same_as_everywhere_else() {
    assert_eq!(capped_grace_period(None, 60), 30);
    assert_eq!(capped_grace_period(None, 10), 10); // ...unless the budget is smaller
}

#[test]
fn zero_budget_caps_to_zero() {
    assert_eq!(capped_grace_period(Some(30), 0), 0);
}

#[test]
fn negative_pod_grace_period_is_treated_as_zero() {
    // Not something the apiserver should ever hand back, but a defensive
    // floor keeps this from producing a negative CRI StopContainer timeout.
    assert_eq!(capped_grace_period(Some(-5), 60), 0);
}
