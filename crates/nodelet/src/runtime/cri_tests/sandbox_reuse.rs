//! sandbox_reuse_decision(): the fix for pods stuck forever with
//! "failed to get sandbox container task: no running task found" after a
//! reboot. containerd's sandbox metadata can persist across a reboot while
//! the sandbox's own task/pause process cannot (processes don't survive
//! one) — find_sandbox() used to be existence-only, so it kept reusing a
//! dead sandbox reference forever, and every CreateContainer against it
//! failed the same way, permanently, for every pod on the node.
use super::*;

const READY: i32 = 0; // matches PodSandboxState::SandboxReady as i32
const NOTREADY: i32 = 1;

#[test]
fn no_sandbox_found_creates_fresh() {
    assert_eq!(sandbox_reuse_decision(None, READY), SandboxDecision::CreateFresh);
}

#[test]
fn ready_sandbox_is_reused() {
    assert_eq!(sandbox_reuse_decision(Some(READY), READY), SandboxDecision::Reuse);
}

#[test]
fn not_ready_sandbox_is_recreated_not_reused() {
    // This is the exact regression: a NOTREADY (dead-task) sandbox must
    // never be silently reused.
    assert_eq!(sandbox_reuse_decision(Some(NOTREADY), READY), SandboxDecision::RecreateStale);
}

#[test]
fn any_state_other_than_ready_counts_as_stale() {
    // Robustness against the CRI spec growing more states later — anything
    // that isn't literally the ready value must not be treated as reusable.
    for weird_state in [2, 99, -1] {
        assert_eq!(sandbox_reuse_decision(Some(weird_state), READY), SandboxDecision::RecreateStale);
    }
}
