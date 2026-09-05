//! next_retry_delay(): the backoff schedule schedule_retry() walks when
//! ensure_pod() keeps failing. Tested as a plain function so the schedule is
//! pinned without spawning the detached task or waiting real seconds.
use super::*;

#[test]
fn teardown_retries_share_one_grace_period_and_leave_time_for_cleanup() {
    let pod: Pod = serde_json::from_value(serde_json::json!({
        "metadata": {"deletionGracePeriodSeconds": 60},
        "spec": {"containers": [], "terminationGracePeriodSeconds": 90},
    })).unwrap();
    assert_eq!(teardown_attempt_budget(&pod, Duration::ZERO),
        (60, Duration::from_secs(120)));
    assert_eq!(teardown_attempt_budget(&pod, Duration::from_millis(59_500)),
        (1, Duration::from_secs(61)));
    assert_eq!(teardown_attempt_budget(&pod, Duration::from_secs(65)),
        (0, TEARDOWN_RUNTIME_TIMEOUT));
}

#[test]
fn force_deleted_pod_does_not_regain_its_spec_grace() {
    let pod: Pod = serde_json::from_value(serde_json::json!({
        "metadata": {"deletionGracePeriodSeconds": 0},
        "spec": {"containers": [], "terminationGracePeriodSeconds": 60},
    })).unwrap();
    assert_eq!(teardown_attempt_budget(&pod, Duration::ZERO),
        (0, TEARDOWN_RUNTIME_TIMEOUT));
}

#[test]
fn first_step_doubles_the_initial_delay() {
    assert_eq!(next_retry_delay(RETRY_FIRST_DELAY), Duration::from_secs(10));
}

#[test]
fn keeps_doubling_below_the_ceiling() {
    assert_eq!(next_retry_delay(Duration::from_secs(10)), Duration::from_secs(20));
    assert_eq!(next_retry_delay(Duration::from_secs(20)), Duration::from_secs(40));
    assert_eq!(next_retry_delay(Duration::from_secs(80)), Duration::from_secs(160));
}

/// The ceiling is what makes retrying-until-fixed affordable — a doubling
/// that would overshoot it clamps rather than skipping past it.
#[test]
fn clamps_at_the_ceiling_instead_of_overshooting() {
    assert_eq!(next_retry_delay(Duration::from_secs(160)), RETRY_MAX_DELAY);
    assert_eq!(next_retry_delay(RETRY_MAX_DELAY), RETRY_MAX_DELAY);
}

/// Once at the ceiling it stays there forever: a permanently broken pod
/// settles at exactly one wakeup every 5 minutes, never drifting up and
/// never overflowing.
#[test]
fn is_a_fixed_point_at_the_ceiling() {
    let mut delay = RETRY_FIRST_DELAY;
    for _ in 0..1000 {
        delay = next_retry_delay(delay);
    }
    assert_eq!(delay, RETRY_MAX_DELAY);
}

/// Reaching the ceiling from the first delay takes a handful of steps, not
/// dozens — the point is to recover promptly after a node-level fix, not to
/// go quiet immediately.
#[test]
fn reaches_the_ceiling_in_a_bounded_number_of_steps() {
    let mut delay = RETRY_FIRST_DELAY;
    let mut steps = 0;
    while delay < RETRY_MAX_DELAY {
        delay = next_retry_delay(delay);
        steps += 1;
    }
    assert_eq!(steps, 6);
}

#[test]
fn projected_service_account_token_waits_are_retried() {
    let status = RuntimeStatus {
        phase: Phase::Pending,
        message: Some(
            "waiting for projected ServiceAccount token(s) to be materialized: api-token"
                .to_string(),
        ),
        started_at: None,
        pod_ip: None,
        containers: Vec::new(),
        init_containers: Vec::new(),
        ephemeral_containers: Vec::new(),
        initialized: false,
    };
    assert!(is_waiting_for_external_resource(&status));
}
