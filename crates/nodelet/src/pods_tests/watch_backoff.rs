//! The pacing curve for a watch that cannot start.
//!
//! These are cheap arithmetic assertions on purpose. The bug they guard was
//! never in the arithmetic — it was that no curve was applied at all, so the
//! watchers busy-looped against a restarting apiserver (128 requests in one
//! second, measured live). What is worth pinning here is the two properties
//! that make the curve the right one: it actually grows, and its ceiling
//! stays low enough that the node resumes reconciling within seconds of the
//! apiserver coming back rather than sleeping through its return.

use super::*;

#[test]
fn no_failures_means_no_delay() {
    // The reset case. A healthy watch must not pay anything for the
    // machinery being present.
    assert_eq!(watch_backoff(0), Duration::ZERO);
}

#[test]
fn the_first_failure_waits_the_initial_delay_then_doubles() {
    assert_eq!(watch_backoff(1), Duration::from_millis(500));
    assert_eq!(watch_backoff(2), Duration::from_secs(1));
    assert_eq!(watch_backoff(3), Duration::from_secs(2));
    assert_eq!(watch_backoff(4), Duration::from_secs(4));
}

#[test]
fn the_curve_is_capped_and_never_overflows() {
    // The ceiling is the load-bearing property: an outage of any length
    // still gets a retry every WATCH_MAX_BACKOFF, so recovery is bounded by
    // the ceiling and not by how long the outage happened to last.
    assert_eq!(watch_backoff(5), WATCH_MAX_BACKOFF);
    assert_eq!(watch_backoff(50), WATCH_MAX_BACKOFF);
    // Well past the point where a naive 1<<n would have overflowed.
    assert_eq!(watch_backoff(u32::MAX), WATCH_MAX_BACKOFF);
}

#[test]
fn a_sustained_outage_retries_often_enough_to_recover_promptly() {
    // Stated as the property that matters rather than as a magic number:
    // whatever the curve is, an apiserver that returns during a long outage
    // must be noticed within a few seconds. kube's own DefaultBackoff caps
    // an order of magnitude higher, which is what makes it the wrong choice
    // here.
    assert!(
        WATCH_MAX_BACKOFF <= Duration::from_secs(5),
        "a ceiling above ~5s means the node can sit idle long after the apiserver is back"
    );
}

#[test]
fn the_policy_advances_on_each_failure_and_resets_on_success() {
    let mut policy = WatchBackoffPolicy::default();
    assert_eq!(policy.next(), Some(Duration::from_millis(500)));
    assert_eq!(policy.next(), Some(Duration::from_secs(1)));
    assert_eq!(policy.next(), Some(Duration::from_secs(2)));

    // Any successful event resets it — without this a watch that fails
    // occasionally would creep up to the ceiling permanently and stay there.
    policy.reset();
    assert_eq!(policy.next(), Some(Duration::from_millis(500)));
}

#[test]
fn the_policy_never_ends_the_stream() {
    // `StreamBackoff` closes the underlying stream when the policy yields
    // None. A watch giving up permanently is exactly the failure this whole
    // change exists to prevent, so the policy must be infinite.
    let mut policy = WatchBackoffPolicy::default();
    for _ in 0..1_000 {
        assert!(policy.next().is_some(), "the backoff policy must never terminate the watch");
    }
}
