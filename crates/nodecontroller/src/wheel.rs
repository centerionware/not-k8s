//! A hashed timing wheel: O(1) insert/cancel/reschedule for any deadline
//! whose count scales with cluster size, in place of a `BinaryHeap`'s
//! `O(log n)` insert/remove and pointer-chasing sift.
//!
//! See `docs/CONTROLLER_MANAGER.md`'s "The mechanism" section for the full
//! rationale (node-lifecycle, podgc, CSR cleaner — one entry per Node/Pod/
//! CSR, rescheduled constantly). Same structure as Linux kernel timers and
//! Netty's `HashedWheelTimer`: a fixed-size ring of slots (a plain array),
//! each holding the keys due in that bucket, with a cursor that advances one
//! slot per tick.
//!
//! Pure and I/O-free by construction — `advance()` takes `now` as a
//! parameter rather than reading the clock itself, the same discipline
//! `nodescheduler::cycle` uses for the scheduling cycle and `nodestore`'s
//! `command.rs` states outright: no hidden clock read, so every transition
//! is unit-testable without waiting on a real timer.
//!
//! # What this does *not* do (yet)
//!
//! This is a **single-revolution** wheel: it has no overflow tier for a
//! deadline further out than `slot_count * tick` (the standard
//! hierarchical-wheel escape hatch). `insert()` returns
//! [`InsertError::BeyondHorizon`] rather than silently truncating or
//! wrapping, so a caller finds out immediately rather than discovering a
//! deadline fired a full revolution early. Nothing in this crate's Tier 0
//! scope (node-lifecycle) has a deadline anywhere near a typical horizon
//! (grace period ~40s), so no consumer needs the overflow tier yet — adding
//! one later is additive, not a rework of what's here.

use std::collections::HashMap;
use std::hash::Hash;
use std::time::{Duration, Instant};

#[derive(Debug, PartialEq, Eq)]
pub enum InsertError {
    /// `deadline` is further from `start` than this wheel's horizon
    /// (`slot_count * tick`) covers.
    BeyondHorizon,
}

pub struct TimingWheel<K: Eq + Hash + Clone> {
    start: Instant,
    tick: Duration,
    slot_count: u64,
    slots: Vec<Vec<K>>,
    /// Which slot each live key currently sits in, for O(1) cancel/move —
    /// this is the "pointer" half of "array + pointer": the array is
    /// `slots`, this map is what lets `cancel`/re-`insert` find a key's
    /// current slot without scanning the whole wheel.
    key_slot: HashMap<K, usize>,
    /// Monotonic tick counter since `start`, never wraps (only `% slot_count`
    /// does, to index into `slots`) — this is what makes "how many ticks
    /// have we swept" unambiguous across arbitrarily many revolutions.
    last_advanced_tick: u64,
}

impl<K: Eq + Hash + Clone> TimingWheel<K> {
    /// `slot_count` slots of `tick` width each — horizon is their product.
    /// `start` is tick 0; every deadline is measured relative to it.
    pub fn new(slot_count: u64, tick: Duration, start: Instant) -> Self {
        assert!(slot_count > 0, "a timing wheel needs at least one slot");
        assert!(!tick.is_zero(), "a timing wheel's tick must be nonzero");
        TimingWheel {
            start,
            tick,
            slot_count,
            slots: (0..slot_count).map(|_| Vec::new()).collect(),
            key_slot: HashMap::new(),
            last_advanced_tick: 0,
        }
    }

    pub fn horizon(&self) -> Duration {
        self.tick * self.slot_count as u32
    }

    /// Which tick number `deadline` belongs to, ceiling-divided so a
    /// deadline exactly on a tick boundary fires *at* that boundary rather
    /// than one early, and clamped to `last_advanced_tick + 1` — never
    /// `last_advanced_tick` itself, which `advance()` never sweeps again
    /// (it only ever advances *past* the last-swept index). Found live by
    /// this file's own tests: a deadline of `now` computed via plain floor
    /// division landed in the slot `advance()` had already passed,
    /// stranding it there until the wheel came full circle a revolution
    /// later instead of firing on the very next tick.
    fn tick_index_for(&self, deadline: Instant) -> u64 {
        let elapsed = deadline.saturating_duration_since(self.start);
        let tick_ns = self.tick.as_nanos().max(1);
        let idx = elapsed.as_nanos().div_ceil(tick_ns) as u64;
        idx.max(self.last_advanced_tick + 1)
    }

    /// Insert `key`, due at `deadline`. Idempotent on `key`: inserting an
    /// already-present key first removes it from its old slot — this is the
    /// O(1) "reschedule", the operation node-lifecycle performs on every
    /// heartbeat, cluster-wide, once per `node-monitor-period`.
    pub fn insert(&mut self, key: K, deadline: Instant) -> Result<(), InsertError> {
        let tick_index = self.tick_index_for(deadline);
        // tick_index is always > last_advanced_tick (tick_index_for's own
        // clamp guarantees it), so a distance of exactly slot_count is a
        // full revolution out and still fits — `>`, not `>=`.
        if tick_index - self.last_advanced_tick > self.slot_count {
            return Err(InsertError::BeyondHorizon);
        }
        self.cancel(&key);
        let slot = (tick_index % self.slot_count) as usize;
        self.slots[slot].push(key.clone());
        self.key_slot.insert(key, slot);
        Ok(())
    }

    /// Remove `key` if present. `true` if it was there.
    pub fn cancel(&mut self, key: &K) -> bool {
        match self.key_slot.remove(key) {
            Some(slot) => {
                self.slots[slot].retain(|k| k != key);
                true
            }
            None => false,
        }
    }

    pub fn contains(&self, key: &K) -> bool {
        self.key_slot.contains_key(key)
    }

    pub fn len(&self) -> usize {
        self.key_slot.len()
    }

    pub fn is_empty(&self) -> bool {
        self.key_slot.is_empty()
    }

    /// Advance the cursor to `now`, sweeping every slot the cursor passes
    /// through and returning their keys in slot order (oldest tick first).
    /// A call after a long gap (the process was starved, or this is the
    /// first call) sweeps every intervening slot in one pass — nothing is
    /// missed, and nothing double-fires.
    pub fn advance(&mut self, now: Instant) -> Vec<K> {
        let elapsed = now.saturating_duration_since(self.start);
        let target_tick = (elapsed.as_nanos() / self.tick.as_nanos().max(1)) as u64;
        let mut due = Vec::new();
        while self.last_advanced_tick < target_tick {
            self.last_advanced_tick += 1;
            let slot = (self.last_advanced_tick % self.slot_count) as usize;
            let drained: Vec<K> = self.slots[slot].drain(..).collect();
            for k in &drained {
                self.key_slot.remove(k);
            }
            due.extend(drained);
        }
        due
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wheel(slot_count: u64, tick_ms: u64) -> (TimingWheel<u32>, Instant) {
        let start = Instant::now();
        (TimingWheel::new(slot_count, Duration::from_millis(tick_ms), start), start)
    }

    #[test]
    fn a_deadline_in_the_past_is_due_on_the_very_next_advance() {
        let (mut w, start) = wheel(10, 100);
        w.insert(1, start).unwrap();
        let due = w.advance(start + Duration::from_millis(100));
        assert_eq!(due, vec![1]);
    }

    #[test]
    fn a_deadline_several_slots_out_is_not_due_early() {
        let (mut w, start) = wheel(10, 100);
        w.insert(1, start + Duration::from_millis(550)).unwrap();
        assert!(w.advance(start + Duration::from_millis(400)).is_empty());
        assert!(w.advance(start + Duration::from_millis(500)).is_empty());
        assert_eq!(w.advance(start + Duration::from_millis(600)), vec![1]);
    }

    #[test]
    fn inserting_an_existing_key_reschedules_it_rather_than_duplicating_it() {
        let (mut w, start) = wheel(10, 100);
        w.insert(1, start + Duration::from_millis(150)).unwrap();
        w.insert(1, start + Duration::from_millis(850)).unwrap();
        assert_eq!(w.len(), 1);
        assert!(w.advance(start + Duration::from_millis(200)).is_empty());
        assert_eq!(w.advance(start + Duration::from_millis(900)), vec![1]);
    }

    #[test]
    fn cancel_removes_a_key_before_it_fires() {
        let (mut w, start) = wheel(10, 100);
        w.insert(1, start + Duration::from_millis(150)).unwrap();
        assert!(w.cancel(&1));
        assert!(!w.cancel(&1)); // already gone
        assert!(w.advance(start + Duration::from_millis(200)).is_empty());
    }

    #[test]
    fn a_deadline_beyond_the_horizon_is_rejected_not_truncated() {
        let (mut w, start) = wheel(10, 100); // horizon = 1000ms
        let err = w.insert(1, start + Duration::from_millis(1500)).unwrap_err();
        assert_eq!(err, InsertError::BeyondHorizon);
        assert!(!w.contains(&1));
    }

    #[test]
    fn a_long_gap_between_advances_sweeps_every_intervening_slot_once() {
        let (mut w, start) = wheel(5, 100);
        w.insert(1, start + Duration::from_millis(50)).unwrap();
        w.insert(2, start + Duration::from_millis(150)).unwrap();
        w.insert(3, start + Duration::from_millis(250)).unwrap();
        // One big jump, as if the governor was starved for a while.
        let mut due = w.advance(start + Duration::from_millis(300));
        due.sort();
        assert_eq!(due, vec![1, 2, 3]);
    }

    #[test]
    fn many_entries_spread_across_slots_do_not_interfere_with_each_other() {
        let (mut w, start) = wheel(8, 100);
        for i in 0..8u32 {
            w.insert(i, start + Duration::from_millis(100 * (i as u64 + 1))).unwrap();
        }
        assert_eq!(w.len(), 8);
        for i in 0..8u32 {
            let due = w.advance(start + Duration::from_millis(100 * (i as u64 + 1)));
            assert_eq!(due, vec![i], "slot {i} fired the wrong entry (or the wrong count)");
        }
        assert!(w.is_empty());
    }
}
