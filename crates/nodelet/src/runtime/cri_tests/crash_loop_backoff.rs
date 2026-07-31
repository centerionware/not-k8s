//! crash_loop_backoff_secs()/crash_loop_backoff_ready(): the pure
//! decision logic behind round 73's crash-loop backoff, matching real
//! kubelet's own flowcontrol.Backoff defaults (10s base, doubling,
//! capped at 5 minutes, resetting after 10 minutes of no restart
//! attempt).
use super::*;

#[test]
fn first_ever_restart_has_no_prior_state_and_is_immediately_ready() {
    assert!(crash_loop_backoff_ready(None, CRASH_LOOP_BACKOFF_BASE_SECS, 1_000_000));
}

#[test]
fn first_ever_backoff_computed_is_the_base_delay() {
    assert_eq!(crash_loop_backoff_secs(None, None), CRASH_LOOP_BACKOFF_BASE_SECS);
}

#[test]
fn a_second_failure_shortly_after_the_first_doubles_the_backoff() {
    // Prior backoff was the base (10s); failing again well within the
    // reset window doubles it.
    let elapsed = Some(5); // failed again before even the 10s window elapsed
    assert_eq!(crash_loop_backoff_secs(Some(CRASH_LOOP_BACKOFF_BASE_SECS), elapsed), 20);
}

#[test]
fn backoff_keeps_doubling_up_to_the_cap() {
    assert_eq!(crash_loop_backoff_secs(Some(20), Some(1)), 40);
    assert_eq!(crash_loop_backoff_secs(Some(160), Some(1)), 300);
}

#[test]
fn backoff_never_exceeds_the_cap_even_starting_above_it() {
    // Shouldn't happen in practice (state is always built by prior calls
    // to this same function), but the cap must hold regardless of input.
    assert_eq!(crash_loop_backoff_secs(Some(300), Some(1)), 300);
}

#[test]
fn a_long_gap_since_the_last_restart_attempt_resets_to_the_base_delay() {
    let elapsed = Some(CRASH_LOOP_BACKOFF_RESET_SECS);
    assert_eq!(crash_loop_backoff_secs(Some(300), elapsed), CRASH_LOOP_BACKOFF_BASE_SECS);
}

#[test]
fn a_gap_just_under_the_reset_threshold_does_not_reset() {
    let elapsed = Some(CRASH_LOOP_BACKOFF_RESET_SECS - 1);
    assert_eq!(crash_loop_backoff_secs(Some(40), elapsed), 80);
}

#[test]
fn ready_returns_false_before_the_backoff_window_elapses() {
    assert!(!crash_loop_backoff_ready(Some(1_000), 30, 1_010));
}

#[test]
fn ready_returns_true_once_the_backoff_window_has_fully_elapsed() {
    assert!(crash_loop_backoff_ready(Some(1_000), 30, 1_030));
}

#[test]
fn ready_is_never_a_panic_if_clocks_look_like_they_went_backwards() {
    // now_unix < last_restart_unix shouldn't happen, but saturating_sub
    // must not panic or wrap if it somehow does.
    assert!(!crash_loop_backoff_ready(Some(1_000), 30, 500));
}
