//! compute_phase(): the direct fix for the coredns pile-up. Reporting
//! Succeeded for a restartPolicy: Always pod whose container merely exited
//! is what made Kubernetes' ReplicaSet controller treat it as permanently
//! inactive and replace it — forever, once per crash. Every branch of this
//! matrix is load-bearing.
use super::*;

#[test]
fn running_container_is_running_regardless_of_policy() {
    for policy in ["Always", "OnFailure", "Never"] {
        assert_eq!(compute_phase(true, false, false, policy), Phase::Running);
        // any_running wins even if all_exited was also (incorrectly) true.
        assert_eq!(compute_phase(true, true, false, policy), Phase::Running);
    }
}

#[test]
fn always_policy_never_reports_succeeded_on_exit() {
    // This is the exact bug: restartPolicy: Always (every Deployment,
    // including coredns) must never see a terminal phase from a container
    // exiting, or the ReplicaSet controller replaces the pod.
    assert_eq!(compute_phase(false, true, false, "Always"), Phase::Pending);
    assert_eq!(compute_phase(false, true, true, "Always"), Phase::Pending);
}

#[test]
fn onfailure_policy_never_reports_succeeded_on_exit() {
    // Always restarted regardless of exit code — a failed container just
    // means "restarted", never "pod done" — so any_failed doesn't matter here.
    assert_eq!(compute_phase(false, true, false, "OnFailure"), Phase::Pending);
    assert_eq!(compute_phase(false, true, true, "OnFailure"), Phase::Pending);
}

#[test]
fn never_policy_all_exited_zero_reports_succeeded() {
    assert_eq!(compute_phase(false, true, false, "Never"), Phase::Succeeded);
}

#[test]
fn never_policy_any_exited_nonzero_reports_failed() {
    // The fix in this round: restartPolicy: Never used to always report
    // Succeeded regardless of exit code — a genuinely failed one-shot pod
    // looked identical to a successful one.
    assert_eq!(compute_phase(false, true, true, "Never"), Phase::Failed);
}

#[test]
fn not_running_and_not_all_exited_is_pending_regardless_of_policy() {
    // Mid-creation: a container exists but hasn't reached Running or
    // Exited yet (e.g. still Created).
    for policy in ["Always", "OnFailure", "Never", "unknown-value"] {
        assert_eq!(compute_phase(false, false, false, policy), Phase::Pending);
    }
}

#[test]
fn unknown_restart_policy_value_is_treated_like_always() {
    // Anything other than the literal "Never" string must fail safe
    // (never Succeeded/Failed) — matches ensure_pod()'s own default of
    // "Always" when the Pod spec omits restartPolicy.
    assert_eq!(compute_phase(false, true, false, ""), Phase::Pending);
    assert_eq!(compute_phase(false, true, false, "never"), Phase::Pending); // case-sensitive on purpose
}
