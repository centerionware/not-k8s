//! Routing a result back to whoever proposed it.
//!
//! A write enters raft as a log entry and comes out, some milliseconds and
//! one quorum later, as an applied entry on every member. The member that
//! *proposed* it has a client waiting on the answer; every other member is
//! applying an entry nobody local asked for. That asymmetry is all this
//! module is.
//!
//! # Why a proposal can fail after being accepted
//!
//! Raft's `propose` means "this entry has been appended to my log", not "this
//! entry will be committed". A leader that is deposed before its entries
//! commit will have them *overwritten* by the new leader — correctly, because
//! they were never committed. The proposer must therefore be told the write
//! did not happen, and the two ways that becomes visible are:
//!
//!   * **losing leadership** — every entry this node proposed and has not yet
//!     applied is now in doubt, and is failed immediately;
//!   * **timeout** — a proposal that neither commits nor visibly fails, e.g.
//!     because quorum was lost and the term never advanced here.
//!
//! Silently leaving a caller hanging would be worse than either: kube-apiserver
//! blocks on its storage write, and a request that never returns takes a
//! worker with it.

use crate::error::Error;
use crate::store::Applied;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use tokio::sync::oneshot;

pub type ProposalResult = std::result::Result<Applied, Error>;

#[derive(Default)]
pub struct ProposalTracker {
    next_id: AtomicU64,
    pending: Mutex<HashMap<u64, oneshot::Sender<ProposalResult>>>,
}

impl ProposalTracker {
    pub fn new() -> ProposalTracker {
        ProposalTracker::default()
    }

    /// Register a waiter and return its id, to be embedded in the log entry.
    pub fn register(&self) -> (u64, oneshot::Receiver<ProposalResult>) {
        // Starts at 1: zero is the id an entry carries when it was not
        // proposed by anyone waiting (raft's own empty entry at the start of
        // a term), and completing "proposal 0" must never match a real
        // waiter.
        let id = self.next_id.fetch_add(1, Ordering::SeqCst) + 1;
        let (tx, rx) = oneshot::channel();
        self.pending.lock().expect("proposals mutex").insert(id, tx);
        (id, rx)
    }

    /// Deliver a result. A missing waiter is the normal case on a follower.
    pub fn complete(&self, id: u64, result: ProposalResult) {
        if id == 0 {
            return;
        }
        let waiter = self.pending.lock().expect("proposals mutex").remove(&id);
        if let Some(tx) = waiter {
            // The receiver being gone means the caller timed out or hung up,
            // which is not an error here — the entry still applied.
            let _ = tx.send(result);
        }
    }

    /// Fail every outstanding proposal. Called on losing leadership.
    pub fn fail_all(&self, reason: &str) -> usize {
        let mut pending = self.pending.lock().expect("proposals mutex");
        let count = pending.len();
        for (_, tx) in pending.drain() {
            let _ = tx.send(Err(Error::Unavailable(reason.to_string())));
        }
        count
    }

    /// Give up on one proposal, e.g. after a timeout, so its slot is not
    /// leaked for the life of the process.
    pub fn forget(&self, id: u64) {
        self.pending.lock().expect("proposals mutex").remove(&id);
    }

    pub fn pending_count(&self) -> usize {
        self.pending.lock().expect("proposals mutex").len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::CommandResponse;

    fn applied(revision: i64) -> Applied {
        Applied { revision, response: CommandResponse::Empty, events: Vec::new() }
    }

    #[tokio::test]
    async fn a_registered_proposal_receives_its_result() {
        let t = ProposalTracker::new();
        let (id, rx) = t.register();
        t.complete(id, Ok(applied(7)));
        assert_eq!(rx.await.unwrap().unwrap().revision, 7);
        assert_eq!(t.pending_count(), 0, "a completed proposal must not leak");
    }

    #[test]
    fn ids_never_start_at_zero() {
        // Zero is what an entry carries when nobody is waiting on it — raft's
        // own empty entry at the start of a term. A real proposal sharing
        // that id would be completed by it.
        let t = ProposalTracker::new();
        for _ in 0..10 {
            let (id, _rx) = t.register();
            assert_ne!(id, 0);
        }
    }

    #[test]
    fn completing_id_zero_matches_nothing() {
        let t = ProposalTracker::new();
        let (_id, _rx) = t.register();
        t.complete(0, Ok(applied(1)));
        assert_eq!(t.pending_count(), 1, "the real proposal must still be waiting");
    }

    #[tokio::test]
    async fn losing_leadership_fails_every_outstanding_proposal() {
        // The case this module exists for: entries proposed but not committed
        // are overwritten by the next leader, so their callers must be told
        // the write did not happen rather than left hanging.
        let t = ProposalTracker::new();
        let (_id1, rx1) = t.register();
        let (_id2, rx2) = t.register();

        assert_eq!(t.fail_all("leadership lost"), 2);
        assert!(matches!(rx1.await.unwrap(), Err(Error::Unavailable(_))));
        assert!(matches!(rx2.await.unwrap(), Err(Error::Unavailable(_))));
        assert_eq!(t.pending_count(), 0);
    }

    #[tokio::test]
    async fn completing_an_unknown_id_is_harmless() {
        // Every follower applying an entry hits this path: the entry carries
        // the proposing node's id, which means nothing here.
        let t = ProposalTracker::new();
        t.complete(12345, Ok(applied(1)));
    }

    #[tokio::test]
    async fn a_dropped_receiver_does_not_break_completion() {
        // The caller timed out and went away; the entry still applied, and
        // the applier must not care.
        let t = ProposalTracker::new();
        let (id, rx) = t.register();
        drop(rx);
        t.complete(id, Ok(applied(3)));
        assert_eq!(t.pending_count(), 0);
    }

    #[test]
    fn forgetting_a_proposal_frees_its_slot() {
        let t = ProposalTracker::new();
        let (id, _rx) = t.register();
        assert_eq!(t.pending_count(), 1);
        t.forget(id);
        assert_eq!(t.pending_count(), 0);
    }
}
