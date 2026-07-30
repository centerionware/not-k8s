//! init_container_decision(): the exact state machine that gates app
//! containers behind init containers finishing in order. Before this, init
//! containers weren't run at all — ensure_pod() only ever looked at
//! spec.containers.
use super::*;

const RUNNING: i32 = 1;
const EXITED: i32 = 2;

#[test]
fn no_existing_container_creates_it() {
    assert_eq!(init_container_decision(None, RUNNING, EXITED, 0, "Always"), InitContainerDecision::Create);
}

#[test]
fn running_container_waits() {
    assert_eq!(
        init_container_decision(Some(RUNNING), RUNNING, EXITED, 0, "Always"),
        InitContainerDecision::StillRunning
    );
}

#[test]
fn exited_zero_is_done_regardless_of_restart_policy() {
    for policy in ["Always", "OnFailure", "Never"] {
        assert_eq!(
            init_container_decision(Some(EXITED), RUNNING, EXITED, 0, policy),
            InitContainerDecision::Done,
            "policy {policy}"
        );
    }
}

#[test]
fn exited_nonzero_with_never_is_failed() {
    assert_eq!(
        init_container_decision(Some(EXITED), RUNNING, EXITED, 1, "Never"),
        InitContainerDecision::Failed
    );
}

#[test]
fn exited_nonzero_with_always_or_onfailure_retries() {
    assert_eq!(
        init_container_decision(Some(EXITED), RUNNING, EXITED, 1, "Always"),
        InitContainerDecision::Retry
    );
    assert_eq!(
        init_container_decision(Some(EXITED), RUNNING, EXITED, 1, "OnFailure"),
        InitContainerDecision::Retry
    );
}

#[test]
fn neither_running_nor_exited_waits() {
    const CREATED: i32 = 0;
    assert_eq!(
        init_container_decision(Some(CREATED), RUNNING, EXITED, 0, "Always"),
        InitContainerDecision::Waiting
    );
}
