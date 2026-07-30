//! compute_phase(): the direct fix for the coredns pile-up. Reporting
//! Succeeded for a restartPolicy: Always pod whose container merely exited
//! is what made Kubernetes' ReplicaSet controller treat it as permanently
//! inactive and replace it — forever, once per crash. Every branch of this
//! matrix is load-bearing.
use super::*;

#[test]
fn running_container_is_running_regardless_of_policy() {
    for policy in ["Always", "OnFailure", "Never"] {
        assert_eq!(compute_phase(true, false, policy), Phase::Running);
        // any_running wins even if all_exited was also (incorrectly) true.
        assert_eq!(compute_phase(true, true, policy), Phase::Running);
    }
}

#[test]
fn always_policy_never_reports_succeeded_on_exit() {
    // This is the exact bug: restartPolicy: Always (every Deployment,
    // including coredns) must never see a terminal phase from a container
    // exiting, or the ReplicaSet controller replaces the pod.
    assert_eq!(compute_phase(false, true, "Always"), Phase::Pending);
}

#[test]
fn onfailure_policy_never_reports_succeeded_on_exit() {
    // Treated the same as Always here — CRI status doesn't expose
    // per-container exit codes, so we can't distinguish "OnFailure but
    // exited zero" from "OnFailure and failed" at this layer.
    assert_eq!(compute_phase(false, true, "OnFailure"), Phase::Pending);
}

#[test]
fn never_policy_reports_succeeded_on_exit() {
    assert_eq!(compute_phase(false, true, "Never"), Phase::Succeeded);
}

#[test]
fn not_running_and_not_all_exited_is_pending_regardless_of_policy() {
    // Mid-creation: a container exists but hasn't reached Running or
    // Exited yet (e.g. still Created).
    for policy in ["Always", "OnFailure", "Never", "unknown-value"] {
        assert_eq!(compute_phase(false, false, policy), Phase::Pending);
    }
}

#[test]
fn unknown_restart_policy_value_is_treated_like_always() {
    // Anything other than the literal "Never" string must fail safe
    // (never Succeeded) — matches ensure_pod()'s own default of "Always"
    // when the Pod spec omits restartPolicy.
    assert_eq!(compute_phase(false, true, ""), Phase::Pending);
    assert_eq!(compute_phase(false, true, "never"), Phase::Pending); // case-sensitive on purpose
}
