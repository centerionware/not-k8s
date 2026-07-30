//! build_pod_status(): the fix for the self-feedback reconcile loop that
//! was hammering the apiserver (commit c1dea26). Stamping every condition
//! with now() unconditionally made every patch_status a real diff, which
//! re-triggered the watch, which re-triggered reconcile(), forever. These
//! tests pin down that an unchanged condition must carry its old timestamp
//! forward, and a genuinely changed one must get a new one.
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
        }],
    }
}

fn pending_status() -> RuntimeStatus {
    RuntimeStatus {
        phase: Phase::Pending,
        message: None,
        started_at: None,
        pod_ip: None,
        containers: vec![ContainerRuntimeStatus {
            name: "app".to_string(),
            image: "busybox".to_string(),
            ready: false,
            running: false,
            container_id: None,
        }],
    }
}

fn fixed_time(seconds_ago: i64) -> Time {
    Time(k8s_openapi::jiff::Timestamp::now() - k8s_openapi::jiff::Span::new().seconds(seconds_ago))
}

#[test]
fn no_prev_status_stamps_fresh_timestamps() {
    let status = build_pod_status("10.0.0.1", &running_status(), None);
    for c in status.conditions.as_ref().unwrap() {
        assert!(c.last_transition_time.is_some());
    }
}

#[test]
fn unchanged_condition_preserves_prior_timestamp() {
    // This is THE regression test for the self-feedback loop: calling this
    // twice with the same underlying RuntimeStatus must produce identical
    // condition timestamps, or every "unchanged" reconcile still generates
    // a real diff against the stored object.
    let old_time = fixed_time(300);
    let prev = PodStatus {
        conditions: Some(vec![PodCondition {
            type_: "Ready".to_string(),
            status: "True".to_string(),
            last_transition_time: Some(old_time.clone()),
            ..Default::default()
        }]),
        ..Default::default()
    };

    let status = build_pod_status("10.0.0.1", &running_status(), Some(&prev));
    let ready = status.conditions.as_ref().unwrap().iter().find(|c| c.type_ == "Ready").unwrap();
    assert_eq!(ready.status, "True"); // still running -> still True
    assert_eq!(ready.last_transition_time, Some(old_time));
}

#[test]
fn changed_condition_gets_a_fresh_timestamp_not_the_old_one() {
    let old_time = fixed_time(300);
    let prev = PodStatus {
        conditions: Some(vec![PodCondition {
            type_: "Ready".to_string(),
            status: "False".to_string(), // was NOT ready
            last_transition_time: Some(old_time.clone()),
            ..Default::default()
        }]),
        ..Default::default()
    };

    // Now running -> Ready flips to True -> must be a *different* timestamp.
    let status = build_pod_status("10.0.0.1", &running_status(), Some(&prev));
    let ready = status.conditions.as_ref().unwrap().iter().find(|c| c.type_ == "Ready").unwrap();
    assert_eq!(ready.status, "True");
    assert_ne!(ready.last_transition_time, Some(old_time));
}

#[test]
fn each_condition_type_is_matched_independently() {
    // Initialized/PodScheduled are always true; ContainersReady/Ready track
    // `running`. A prev status with only some condition types present must
    // not cross-contaminate types it has no entry for.
    let old_time = fixed_time(60);
    let prev = PodStatus {
        conditions: Some(vec![PodCondition {
            type_: "Initialized".to_string(),
            status: "True".to_string(),
            last_transition_time: Some(old_time.clone()),
            ..Default::default()
        }]),
        ..Default::default()
    };
    let status = build_pod_status("10.0.0.1", &running_status(), Some(&prev));
    let conds = status.conditions.as_ref().unwrap();
    let initialized = conds.iter().find(|c| c.type_ == "Initialized").unwrap();
    assert_eq!(initialized.last_transition_time, Some(old_time));
    // Ready has no prior entry to match — must still get a value, just a fresh one.
    let ready = conds.iter().find(|c| c.type_ == "Ready").unwrap();
    assert!(ready.last_transition_time.is_some());
}

#[test]
fn container_creating_reason_set_when_not_running() {
    let status = build_pod_status("10.0.0.1", &pending_status(), None);
    let cs = &status.container_statuses.as_ref().unwrap()[0];
    let waiting = cs.state.as_ref().unwrap().waiting.as_ref().expect("expected waiting state");
    assert_eq!(waiting.reason.as_deref(), Some("ContainerCreating"));
    assert!(cs.state.as_ref().unwrap().running.is_none());
}

#[test]
fn running_container_has_running_state_not_waiting() {
    let status = build_pod_status("10.0.0.1", &running_status(), None);
    let cs = &status.container_statuses.as_ref().unwrap()[0];
    assert!(cs.state.as_ref().unwrap().running.is_some());
    assert!(cs.state.as_ref().unwrap().waiting.is_none());
}

#[test]
fn host_ip_is_always_set_from_the_argument() {
    let status = build_pod_status("192.168.1.50", &running_status(), None);
    assert_eq!(status.host_ip.as_deref(), Some("192.168.1.50"));
}

#[test]
fn pod_ip_and_pod_ips_are_populated_when_present() {
    let status = build_pod_status("10.0.0.1", &running_status(), None);
    assert_eq!(status.pod_ip.as_deref(), Some("10.42.0.2"));
    assert_eq!(status.pod_ips.as_ref().unwrap()[0].ip, "10.42.0.2");
}

#[test]
fn pod_ip_absent_when_runtime_has_none() {
    let status = build_pod_status("10.0.0.1", &pending_status(), None);
    assert!(status.pod_ip.is_none());
    assert!(status.pod_ips.is_none());
}

#[test]
fn phase_string_matches_runtime_phase() {
    let status = build_pod_status("10.0.0.1", &running_status(), None);
    assert_eq!(status.phase.as_deref(), Some("Running"));
    let status = build_pod_status("10.0.0.1", &pending_status(), None);
    assert_eq!(status.phase.as_deref(), Some("Pending"));
}

#[test]
fn restart_count_is_always_zero() {
    // Known limitation, not a bug this suite is asserting is correct
    // behavior — pinned so a future change to this notices it's touching
    // a real gap (nodelet doesn't track real restart counts yet) rather
    // than silently "fixing" it as a side effect of something else.
    let status = build_pod_status("10.0.0.1", &running_status(), None);
    assert_eq!(status.container_statuses.as_ref().unwrap()[0].restart_count, 0);
}

#[test]
fn container_ready_mirrors_running() {
    let status = build_pod_status("10.0.0.1", &running_status(), None);
    assert!(status.container_statuses.as_ref().unwrap()[0].ready);
    let status = build_pod_status("10.0.0.1", &pending_status(), None);
    assert!(!status.container_statuses.as_ref().unwrap()[0].ready);
}

#[test]
fn container_id_is_carried_through() {
    let status = build_pod_status("10.0.0.1", &running_status(), None);
    assert_eq!(status.container_statuses.as_ref().unwrap()[0].container_id.as_deref(), Some("abc123"));
}
