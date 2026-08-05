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
    assert_eq!(sandbox_reuse_decision(None, READY, true), SandboxDecision::CreateFresh);
}

#[test]
fn ready_sandbox_with_matching_uid_is_reused() {
    assert_eq!(sandbox_reuse_decision(Some(READY), READY, true), SandboxDecision::Reuse);
}

#[test]
fn not_ready_sandbox_is_recreated_not_reused() {
    // This is the exact regression: a NOTREADY (dead-task) sandbox must
    // never be silently reused.
    assert_eq!(sandbox_reuse_decision(Some(NOTREADY), READY, true), SandboxDecision::RecreateStale);
}

#[test]
fn any_state_other_than_ready_counts_as_stale() {
    // Robustness against the CRI spec growing more states later — anything
    // that isn't literally the ready value must not be treated as reusable.
    for weird_state in [2, 99, -1] {
        assert_eq!(sandbox_reuse_decision(Some(weird_state), READY, true), SandboxDecision::RecreateStale);
    }
}

// --- uid_matches (found live: a StatefulSet pod recreated with a new UID
// after scale-to-0/scale-to-1 kept reusing its previous incarnation's
// Ready sandbox forever — including one built before a privileged-sandbox
// fix landed, so CreateContainer for its privileged container kept failing
// with "no privileged container allowed in sandbox" no matter how many
// times the already-fixed code ran) ---

#[test]
fn ready_sandbox_with_mismatched_uid_is_recreated_not_reused() {
    assert_eq!(sandbox_reuse_decision(Some(READY), READY, false), SandboxDecision::RecreateStale);
}

#[test]
fn mismatched_uid_overrides_ready_state_every_time() {
    // Regardless of how "ready" the old sandbox looks, a UID mismatch means
    // it was built for a different pod object and must never be reused.
    for state in [READY, NOTREADY, 2, 99, -1] {
        assert_eq!(sandbox_reuse_decision(Some(state), READY, false), SandboxDecision::RecreateStale);
    }
}
