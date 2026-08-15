//! The CPU-budgeted governor: owns the timing wheel, ticks at a fixed
//! period (the game-loop-derived "fixed timestep" from
//! docs/CONTROLLER_MANAGER.md), and caps how much processing time it will
//! spend draining due work in any one tick.
//!
//! This is the second half of the mechanism `wheel.rs` provides the first
//! half of. The wheel bounds *which* deadlines are due; this module bounds
//! the *cost* of processing them once they are — a thundering-herd burst
//! (every Node's heartbeat check landing in the same tick after a
//! control-plane restart) still needs a backstop even with O(1) wheel
//! operations, because the O(1) is per-entry, and a burst can still have a
//! lot of entries.

use crate::jitter::jitter;
use crate::wheel::{InsertError, TimingWheel};
use std::collections::VecDeque;
use std::hash::Hash;
use std::time::{Duration, Instant};

/// Pure partition of a batch of due keys into what fits this tick's budget
/// and what has to wait for the next one. `costs` is the measured
/// processing time for each key, in the same order as `due` — the caller
/// (an async loop) measures those; this function only does the arithmetic,
/// so the actual "when do we stop" decision is unit-testable without
/// spinning up any real work.
///
/// Greedy in arrival order deliberately, not by cost: this is pacing, not
/// scheduling — a small entry that arrives after a large one still waits,
/// which keeps per-key latency bounded by "how many ticks since it became
/// due", not by how the rest of the batch happened to sort.
pub fn partition_by_budget<K: Clone>(due: &[K], costs: &[Duration], budget: Duration) -> (Vec<K>, Vec<K>) {
    debug_assert_eq!(due.len(), costs.len());
    let mut spent = Duration::ZERO;
    let mut processed = Vec::with_capacity(due.len());
    let mut deferred = Vec::new();
    for (i, key) in due.iter().enumerate() {
        let cost = costs.get(i).copied().unwrap_or_default();
        if !processed.is_empty() && spent + cost > budget {
            deferred.push(key.clone());
            continue;
        }
        spent += cost;
        processed.push(key.clone());
    }
    (processed, deferred)
}

/// `budget_percent` of one core, expressed as a wall-clock allowance per
/// `tick_period` — e.g. 0.6% of a 100ms tick is 600µs. This is a coarse
/// proxy for real CPU time (it measures wall-clock time spent inside the
/// governor's own processing calls, not `CLOCK_THREAD_CPUTIME_ID`), which is
/// the right level of precision here: these calls are dominated by
/// synchronous decode/decision work with a network await in the middle, and
/// tokio yields the executor during that await rather than spinning it — see
/// docs/CONTROLLER_MANAGER.md's mechanism section for why sub-ms precision
/// (which *would* need real CPU-time sampling) is explicitly not the goal.
pub fn budget_for_tick(budget_percent: f64, tick_period: Duration) -> Duration {
    tick_period.mul_f64((budget_percent / 100.0).clamp(0.0, 1.0))
}

/// Owns one wheel plus the deferred-overflow queue, and jitters every
/// insert. Generic over the key type so node-lifecycle, podgc, and CSR
/// cleaner can each instantiate their own governor without sharing mutable
/// state — see `docs/CONTROLLER_MANAGER.md`: these have unrelated horizons
/// and cardinalities, so one wheel per controller, not one global wheel.
pub struct Governor<K: Eq + Hash + Clone> {
    wheel: TimingWheel<K>,
    /// Work that didn't fit a previous tick's budget. Drained *before* the
    /// wheel's own newly-due entries each tick, so nothing already waiting
    /// gets pushed back arbitrarily far by a steady stream of fresh work —
    /// FIFO across ticks, not just within one.
    deferred: VecDeque<K>,
    tick_period: Duration,
    budget: Duration,
    jitter_fraction: f64,
}

impl<K: Eq + Hash + Clone> Governor<K> {
    pub fn new(slot_count: u64, wheel_tick: Duration, start: Instant, tick_period: Duration, budget_percent: f64, jitter_fraction: f64) -> Self {
        Governor {
            wheel: TimingWheel::new(slot_count, wheel_tick, start),
            deferred: VecDeque::new(),
            tick_period,
            budget: budget_for_tick(budget_percent, tick_period),
            jitter_fraction,
        }
    }

    /// Insert `key`, due at `base_deadline` jittered by `sample` (a caller-
    /// supplied random value in `[-1.0, 1.0]` — see `jitter.rs`'s own doc
    /// comment for why the randomness is passed in rather than drawn here).
    pub fn insert_jittered(&mut self, key: K, base_deadline: Instant, interval: Duration, sample: f64) -> Result<(), InsertError> {
        let jittered = jitter(interval, self.jitter_fraction, sample);
        // jitter() returns a duration around `interval`; the actual deadline
        // is "now-relative-base" shifted by the difference from the
        // unjittered interval, which for every caller in this crate is
        // simply base_deadline adjusted by (jittered - interval).
        let delta = jittered.as_secs_f64() - interval.as_secs_f64();
        let deadline = if delta >= 0.0 {
            base_deadline + Duration::from_secs_f64(delta)
        } else {
            base_deadline.checked_sub(Duration::from_secs_f64(-delta)).unwrap_or(base_deadline)
        };
        self.wheel.insert(key, deadline)
    }

    pub fn cancel(&mut self, key: &K) -> bool {
        self.wheel.cancel(key)
    }

    pub fn contains(&self, key: &K) -> bool {
        self.wheel.contains(key)
    }

    pub fn tick_period(&self) -> Duration {
        self.tick_period
    }

    pub fn budget(&self) -> Duration {
        self.budget
    }

    /// One tick: pull deferred-then-newly-due keys, hand them to the caller.
    /// The caller is responsible for timing its own processing and calling
    /// [`Governor::requeue_overflow`] with anything that didn't fit — kept
    /// as two steps rather than one closure so the budget arithmetic
    /// ([`partition_by_budget`]) stays pure and independently testable.
    pub fn due_this_tick(&mut self, now: Instant) -> Vec<K> {
        let mut due: Vec<K> = self.deferred.drain(..).collect();
        due.extend(self.wheel.advance(now));
        due
    }

    /// Whatever `partition_by_budget` deferred goes back on the queue, in
    /// order, to be tried again next tick before any newly-due work.
    pub fn requeue_overflow(&mut self, overflow: Vec<K>) {
        self.deferred.extend(overflow);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn everything_fits_when_total_cost_is_under_budget() {
        let due = vec![1, 2, 3];
        let costs = vec![Duration::from_micros(10); 3];
        let (processed, deferred) = partition_by_budget(&due, &costs, Duration::from_micros(100));
        assert_eq!(processed, vec![1, 2, 3]);
        assert!(deferred.is_empty());
    }

    #[test]
    fn overflow_is_deferred_once_the_budget_is_spent() {
        let due = vec![1, 2, 3];
        let costs = vec![Duration::from_micros(40), Duration::from_micros(40), Duration::from_micros(40)];
        let (processed, deferred) = partition_by_budget(&due, &costs, Duration::from_micros(100));
        assert_eq!(processed, vec![1, 2]);
        assert_eq!(deferred, vec![3]);
    }

    #[test]
    fn at_least_one_entry_is_always_processed_even_if_it_alone_exceeds_budget() {
        // Otherwise a single very expensive reconcile stalls the queue
        // forever — never processed, never counted as spent, deferred every
        // tick to a next tick that has the exact same problem.
        let due = vec![1];
        let costs = vec![Duration::from_millis(50)];
        let (processed, deferred) = partition_by_budget(&due, &costs, Duration::from_micros(1));
        assert_eq!(processed, vec![1]);
        assert!(deferred.is_empty());
    }

    #[test]
    fn budget_for_tick_scales_with_percent_and_period() {
        let b = budget_for_tick(1.0, Duration::from_millis(100));
        assert_eq!(b, Duration::from_micros(1000)); // 1% of 100ms
        let b2 = budget_for_tick(0.3, Duration::from_millis(100));
        assert_eq!(b2, Duration::from_micros(300));
    }

    #[test]
    fn budget_percent_is_clamped_to_a_sane_range() {
        assert_eq!(budget_for_tick(0.0, Duration::from_millis(100)), Duration::ZERO);
        assert_eq!(budget_for_tick(1000.0, Duration::from_millis(100)), Duration::from_millis(100));
    }

    #[test]
    fn deferred_work_is_returned_before_newly_due_work() {
        let start = Instant::now();
        let mut g: Governor<u32> = Governor::new(10, Duration::from_millis(100), start, Duration::from_millis(100), 1.0, 0.0);
        g.insert_jittered(2, start + Duration::from_millis(100), Duration::from_millis(100), 0.0).unwrap();
        g.requeue_overflow(vec![1]); // simulates "1" not fitting last tick's budget
        let due = g.due_this_tick(start + Duration::from_millis(100));
        assert_eq!(due, vec![1, 2]);
    }
}
