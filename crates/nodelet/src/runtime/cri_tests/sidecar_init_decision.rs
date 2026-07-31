//! sidecar_init_decision(): native sidecar containers (round 36;
//! initContainers[].restartPolicy: "Always") — unlike a regular init
//! container, a sidecar never blocks later containers on its own exit,
//! only on having been created at all.
use super::*;

const RUNNING: i32 = 7; // arbitrary stand-in for ContainerState::ContainerRunning as i32
const EXITED: i32 = 3; // arbitrary stand-in for ContainerState::ContainerExited as i32
const CREATED: i32 = 0;

#[test]
fn no_existing_container_needs_creating() {
    assert_eq!(sidecar_init_decision(None, RUNNING, EXITED), SidecarInitDecision::Create);
}

#[test]
fn a_running_sidecar_is_already_started() {
    assert_eq!(sidecar_init_decision(Some(RUNNING), RUNNING, EXITED), SidecarInitDecision::Started);
}

#[test]
fn an_exited_sidecar_needs_restarting() {
    assert_eq!(sidecar_init_decision(Some(EXITED), RUNNING, EXITED), SidecarInitDecision::NeedsRestart);
}

#[test]
fn a_transient_state_like_created_does_not_block_later_containers() {
    // Unlike a regular init container (which would wait), a sidecar in
    // some other transient CRI state must not re-block the sequence —
    // it's already been created, that's the only gate.
    assert_eq!(sidecar_init_decision(Some(CREATED), RUNNING, EXITED), SidecarInitDecision::Started);
}
