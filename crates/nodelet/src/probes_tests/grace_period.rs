//! probe_grace_period_seconds(): a probe's own terminationGracePeriodSeconds
//! override vs. the pod's own (Round 44; found in round 35's re-audit).
use super::*;
use k8s_openapi::api::core::v1::Probe;

fn probe(termination_grace_period_seconds: Option<i64>) -> Probe {
    Probe { termination_grace_period_seconds, ..Default::default() }
}

#[test]
fn no_override_uses_the_pods_grace_period() {
    assert_eq!(probe_grace_period_seconds(&probe(None), 30), 30);
}

#[test]
fn an_explicit_override_wins_over_the_pods_grace_period() {
    assert_eq!(probe_grace_period_seconds(&probe(Some(5)), 30), 5);
}

#[test]
fn a_zero_override_means_immediate_kill_not_the_pods_default() {
    assert_eq!(probe_grace_period_seconds(&probe(Some(0)), 30), 0);
}

#[test]
fn a_negative_override_is_ignored_in_favor_of_the_pods_grace_period() {
    // The API otherwise allows a negative terminationGracePeriodSeconds
    // through — same defensive handling termination_grace_seconds() (the
    // pod-level equivalent) already applies.
    assert_eq!(probe_grace_period_seconds(&probe(Some(-1)), 30), 30);
}
