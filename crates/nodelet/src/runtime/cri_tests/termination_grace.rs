//! termination_grace_seconds(): before this, remove_pod() called
//! StopPodSandbox directly with no per-container grace period or preStop
//! hook at all — every pod was torn down the same way regardless of
//! terminationGracePeriodSeconds or a defined preStop hook.
use super::*;
use k8s_openapi::api::core::v1::PodSpec;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;

fn pod_with_grace(seconds: Option<i64>) -> Pod {
    Pod {
        spec: Some(PodSpec {
            termination_grace_period_seconds: seconds,
            ..Default::default()
        }),
        ..Default::default()
    }
}

#[test]
fn unset_defaults_to_thirty_seconds() {
    assert_eq!(termination_grace_seconds(&pod_with_grace(None)), 30);
}

#[test]
fn explicit_value_is_honored() {
    assert_eq!(termination_grace_seconds(&pod_with_grace(Some(5))), 5);
}

#[test]
fn zero_is_honored_not_treated_as_unset() {
    assert_eq!(termination_grace_seconds(&pod_with_grace(Some(0))), 0);
}

#[test]
fn negative_falls_back_to_the_default() {
    assert_eq!(termination_grace_seconds(&pod_with_grace(Some(-1))), 30);
}

#[test]
fn no_spec_at_all_defaults_to_thirty_seconds() {
    assert_eq!(termination_grace_seconds(&Pod::default()), 30);
}

#[test]
fn deletion_grace_period_override_wins_over_the_pod_spec() {
    let pod = Pod {
        metadata: ObjectMeta {
            deletion_grace_period_seconds: Some(3),
            ..Default::default()
        },
        spec: Some(PodSpec {
            termination_grace_period_seconds: Some(30),
            ..Default::default()
        }),
        ..Default::default()
    };
    assert_eq!(termination_grace_seconds(&pod), 3);
}

#[test]
fn deletion_grace_period_zero_is_honored() {
    let pod = Pod {
        metadata: ObjectMeta {
            deletion_grace_period_seconds: Some(0),
            ..Default::default()
        },
        spec: Some(PodSpec {
            termination_grace_period_seconds: Some(30),
            ..Default::default()
        }),
        ..Default::default()
    };
    assert_eq!(termination_grace_seconds(&pod), 0);
}
