//! restart_decision(): the other half of the coredns pile-up fix.
//! ensure_container()'s old "already have a container with this name"
//! check matched by name alone, regardless of state, so a crashed
//! container was never restarted. This is the matrix that replaced it.
use super::*;

const RUNNING: i32 = 7; // arbitrary stand-in for ContainerState::ContainerRunning as i32
const EXITED: i32 = 3; // arbitrary stand-in for ContainerState::ContainerExited as i32
const CREATED: i32 = 0;

#[test]
fn no_existing_container_needs_restart() {
    // Same code path as a genuine restart: nothing to remove, just create.
    for policy in ["Always", "OnFailure", "Never"] {
        assert_eq!(restart_decision(None, CREATED, RUNNING, policy), RestartDecision::NeedsRestart);
    }
}

#[test]
fn running_container_is_left_alone_regardless_of_policy() {
    for policy in ["Always", "OnFailure", "Never"] {
        assert_eq!(
            restart_decision(Some(RUNNING), CREATED, RUNNING, policy),
            RestartDecision::AlreadyRunning
        );
    }
}

#[test]
fn exited_container_with_always_needs_restart() {
    assert_eq!(
        restart_decision(Some(EXITED), CREATED, RUNNING, "Always"),
        RestartDecision::NeedsRestart
    );
}

#[test]
fn exited_container_with_onfailure_needs_restart() {
    assert_eq!(
        restart_decision(Some(EXITED), CREATED, RUNNING, "OnFailure"),
        RestartDecision::NeedsRestart
    );
}

#[test]
fn exited_container_with_never_is_left_terminated() {
    // Job-style one-shot semantics: it ran, it's done, don't restart it.
    assert_eq!(
        restart_decision(Some(EXITED), CREATED, RUNNING, "Never"),
        RestartDecision::LeaveTerminated
    );
}

#[test]
fn created_but_not_yet_started_container_starts_in_place_for_every_policy() {
    for policy in ["Always", "OnFailure", "Never"] {
        assert_eq!(
            restart_decision(Some(CREATED), CREATED, RUNNING, policy),
            RestartDecision::StartCreated
        );
    }
}

#[test]
fn decision_is_never_running_for_a_state_that_isnt_literally_running_value() {
    // Regression guard: this must be an exact equality check against the
    // real "running" state constant, not a truthiness/nonzero check —
    // CREATED (0) must not accidentally compare equal to some other
    // "zero means running" assumption.
    assert_ne!(restart_decision(Some(CREATED), CREATED, RUNNING, "Always"), RestartDecision::AlreadyRunning);
}
