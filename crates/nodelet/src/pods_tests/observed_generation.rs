//! condition_observed_generation() (round 87; found in round 86's
//! re-audit): PodCondition.observedGeneration -- real kubelet's own
//! podutil.CalculatePodConditionObservedGeneration semantics.
use super::*;
use crate::runtime::{ContainerRuntimeStatus, Phase, RuntimeStatus};

fn running_status() -> RuntimeStatus {
    RuntimeStatus {
        phase: Phase::Running,
        message: None,
        started_at: None,
        pod_ip: Some("10.42.0.2".to_string()),
        containers: vec![ContainerRuntimeStatus {
            name: "app".to_string(),
            image: "busybox".to_string(),
            ready: true,
            running: true,
            container_id: Some("abc123".to_string()),
            restart_count: 0,
            ..Default::default()
        }],
        init_containers: Vec::new(),
        ephemeral_containers: Vec::new(),
        initialized: true,
    }
}

fn bps_with_generation(rt: &RuntimeStatus, prev: Option<&PodStatus>, generation: Option<i64>) -> PodStatus {
    build_pod_status("10.0.0.1", "default", "app", rt, prev, &[], &probes::new_health_map(), crate::eviction::QosClass::BestEffort, generation)
}

#[test]
fn no_previous_condition_stamps_the_current_generation() {
    assert_eq!(condition_observed_generation(None, "True", Some(5)), Some(5));
}

#[test]
fn unchanged_status_keeps_the_old_observed_generation() {
    let prev = PodCondition { status: "True".to_string(), observed_generation: Some(2), ..Default::default() };
    // Pod is now at generation 9, but the condition's own status hasn't
    // changed since generation 2 -- must NOT bump to 9.
    assert_eq!(condition_observed_generation(Some(&prev), "True", Some(9)), Some(2));
}

#[test]
fn a_status_flip_stamps_the_current_generation() {
    let prev = PodCondition { status: "False".to_string(), observed_generation: Some(2), ..Default::default() };
    assert_eq!(condition_observed_generation(Some(&prev), "True", Some(9)), Some(9));
}

#[test]
fn no_generation_known_produces_none() {
    assert_eq!(condition_observed_generation(None, "True", None), None);
}

#[test]
fn build_pod_status_stamps_new_conditions_with_the_pods_generation() {
    let status = bps_with_generation(&running_status(), None, Some(3));
    for c in status.conditions.as_ref().unwrap() {
        assert_eq!(c.observed_generation, Some(3), "condition {} should be stamped with the pod's current generation", c.type_);
    }
}

#[test]
fn build_pod_status_keeps_the_old_observed_generation_for_an_unchanged_condition() {
    let prev = PodStatus {
        conditions: Some(vec![PodCondition {
            type_: "Ready".to_string(),
            status: "True".to_string(),
            observed_generation: Some(1),
            ..Default::default()
        }]),
        ..Default::default()
    };
    // Still Running -> Ready stays True -> observedGeneration must stay 1
    // even though the pod is now at generation 7.
    let status = bps_with_generation(&running_status(), Some(&prev), Some(7));
    let ready = status.conditions.as_ref().unwrap().iter().find(|c| c.type_ == "Ready").unwrap();
    assert_eq!(ready.observed_generation, Some(1));
}

#[test]
fn build_pod_status_bumps_observed_generation_when_a_condition_actually_flips() {
    let prev = PodStatus {
        conditions: Some(vec![PodCondition {
            type_: "Ready".to_string(),
            status: "False".to_string(),
            observed_generation: Some(1),
            ..Default::default()
        }]),
        ..Default::default()
    };
    // Was not running (Ready: False) -> now Running (Ready: True) -> a
    // genuine status flip must bump observedGeneration to the current one.
    let status = bps_with_generation(&running_status(), Some(&prev), Some(7));
    let ready = status.conditions.as_ref().unwrap().iter().find(|c| c.type_ == "Ready").unwrap();
    assert_eq!(ready.status, "True");
    assert_eq!(ready.observed_generation, Some(7));
}
