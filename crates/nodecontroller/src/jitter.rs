//! Jitter on insert, so correlated deadlines don't bunch into the same
//! wheel slot — see docs/CONTROLLER_MANAGER.md's "Jitter on insert" section.
//! Every Node renews its heartbeat on the same `node-monitor-period`, so
//! without this every Node's next-expiry-check deadline would land in
//! near-identical slots; jitter fans them out.

use std::time::Duration;

/// Given a base `interval` and a random `sample` in `[-1.0, 1.0]`, returns
/// `interval` shifted by up to `fraction` of itself. `sample` is supplied by
/// the caller (drawn from `rand`, typically) rather than read from a global
/// RNG here — the same discipline `nodescheduler::cycle`'s pure-over-a-
/// snapshot invariant uses for its own randomness (reservoir sampling,
/// preemption's candidate offset): nondeterminism is resolved by the caller
/// and passed in, so this function itself stays a plain, pinned-input unit
/// test.
pub fn jitter(interval: Duration, fraction: f64, sample: f64) -> Duration {
    let sample = sample.clamp(-1.0, 1.0);
    let fraction = fraction.clamp(0.0, 1.0);
    let delta = interval.as_secs_f64() * fraction * sample;
    Duration::from_secs_f64((interval.as_secs_f64() + delta).max(0.0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_zero_sample_leaves_the_interval_unchanged() {
        assert_eq!(jitter(Duration::from_secs(40), 0.05, 0.0), Duration::from_secs(40));
    }

    #[test]
    fn a_positive_sample_extends_the_interval_by_up_to_the_fraction() {
        let got = jitter(Duration::from_secs(40), 0.05, 1.0);
        assert_eq!(got, Duration::from_secs_f64(42.0)); // 40 * 1.05
    }

    #[test]
    fn a_negative_sample_shortens_the_interval_by_up_to_the_fraction() {
        let got = jitter(Duration::from_secs(40), 0.05, -1.0);
        assert_eq!(got, Duration::from_secs_f64(38.0)); // 40 * 0.95
    }

    #[test]
    fn the_result_never_goes_negative_even_with_a_large_fraction() {
        let got = jitter(Duration::from_secs(1), 1.0, -1.0);
        assert!(got >= Duration::ZERO);
    }

    #[test]
    fn out_of_range_inputs_are_clamped_not_panicking() {
        let _ = jitter(Duration::from_secs(10), 5.0, 5.0);
        let _ = jitter(Duration::from_secs(10), -1.0, -5.0);
    }
}
