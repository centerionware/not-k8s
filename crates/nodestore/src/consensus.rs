//! Where an ordering decision is made, so that raft can later make it
//! differently without anything else changing.
//!
//! # The shape of the seam
//!
//! Every mutation takes exactly one path:
//!
//! ```text
//!   gRPC handler
//!       └─ Node::propose(Command)
//!            ├─ Consensus::commit(&cmd)   ← decides WHEN the command is in the log
//!            └─ Node::apply_committed()   ← deterministic state machine, identical on every replica
//!                 ├─ Store::apply()       (one sqlite transaction)
//!                 └─ WatchHub::publish()  (fan-out, no polling)
//! ```
//!
//! [`SingleNode`] makes `commit` a no-op: there is nobody to agree with, so a
//! command is committed the moment it is proposed, and the mutex around the
//! store provides the total order. Under raft, `commit` becomes "append to the
//! log, replicate, wait for a quorum, return the assigned index", and
//! `apply_committed` moves off the proposing task onto the single applier that
//! walks the committed log in index order — the proposer then waits for its
//! index instead of applying it itself.
//!
//! What matters is that **nothing above this file knows which of those is
//! happening**. The gRPC layer proposes commands; it does not know whether a
//! quorum was involved.
//!
//! # Why the state machine, not the proposer, assigns revisions
//!
//! A revision is assigned inside [`crate::store::Store::apply`], from the
//! store's own counter, as a function of the command sequence alone. Two
//! replicas applying the same log therefore produce identical revisions
//! without communicating about them. Had the proposer stamped a revision onto
//! the command instead — the obvious shortcut on a single node — every replica
//! would need the leader to also be the only writer, forever, and the
//! single-node implementation would have quietly become the design.

use crate::command::Command;
use crate::error::{Error, Result};
use crate::store::{Applied, Store};
use crate::watch::WatchHub;
use async_trait::async_trait;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};

/// Ordering of commands. The one thing a multi-node deployment changes.
#[async_trait]
pub trait Consensus: Send + Sync {
    /// Return once `cmd` is committed, yielding its log index.
    ///
    /// For raft this is where replication and the quorum wait live. Returning
    /// an error means the command is *not* in the log and had no effect.
    async fn commit(&self, cmd: &Command) -> Result<u64>;

    /// Whether this member may serve writes. Reads that must be linearizable
    /// also require it.
    fn is_leader(&self) -> bool;

    /// Raft term, reported in every response header. Zero without raft.
    fn term(&self) -> u64;

    fn member_id(&self) -> u64;
    fn cluster_id(&self) -> u64;
}

/// The single-node implementation: everything is committed immediately,
/// because there is no one else to convince.
pub struct SingleNode {
    index: AtomicI64,
    member_id: u64,
    cluster_id: u64,
}

impl SingleNode {
    pub fn new(member_id: u64, cluster_id: u64) -> Self {
        SingleNode { index: AtomicI64::new(0), member_id, cluster_id }
    }
}

#[async_trait]
impl Consensus for SingleNode {
    async fn commit(&self, _cmd: &Command) -> Result<u64> {
        // The index still advances, and is still handed to the applier, even
        // though nothing consumes it yet. It is the value raft will supply,
        // and a code path that only exists once raft lands is a code path that
        // has never run.
        Ok(self.index.fetch_add(1, Ordering::SeqCst) as u64 + 1)
    }

    fn is_leader(&self) -> bool {
        true
    }

    fn term(&self) -> u64 {
        0
    }

    fn member_id(&self) -> u64 {
        self.member_id
    }

    fn cluster_id(&self) -> u64 {
        self.cluster_id
    }
}

/// A running datastore: the store, the consensus decision, and the watch
/// fan-out, bound together.
pub struct Node {
    store: Mutex<Store>,
    consensus: Box<dyn Consensus>,
    watch: WatchHub,
    /// Source of lease IDs. Leader-side on purpose — see the determinism note
    /// in [`crate::command`]: the id is chosen *before* the command enters the
    /// log so every replica applies the same one.
    next_lease_id: AtomicI64,
}

impl Node {
    pub fn new(store: Store, consensus: Box<dyn Consensus>, watch_capacity: usize) -> Arc<Node> {
        // Seeded from the clock so ids don't repeat across restarts, which
        // would let a client's stale keepalive refresh a *different* lease
        // that happens to have been handed the same id.
        let seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as i64)
            .unwrap_or(1)
            .abs()
            .max(1);
        Arc::new(Node {
            store: Mutex::new(store),
            consensus,
            watch: WatchHub::new(watch_capacity),
            next_lease_id: AtomicI64::new(seed),
        })
    }

    pub fn watch_hub(&self) -> &WatchHub {
        &self.watch
    }

    pub fn term(&self) -> u64 {
        self.consensus.term()
    }

    pub fn member_id(&self) -> u64 {
        self.consensus.member_id()
    }

    pub fn cluster_id(&self) -> u64 {
        self.consensus.cluster_id()
    }

    pub fn is_leader(&self) -> bool {
        self.consensus.is_leader()
    }

    /// Allocate a lease id. Never called during apply.
    pub fn allocate_lease_id(&self) -> i64 {
        // i64::MAX wraps to a negative id, which etcd treats as invalid, so
        // keep it in the positive half.
        self.next_lease_id.fetch_add(1, Ordering::SeqCst) & 0x7fff_ffff_ffff_ffff
    }

    /// Read under the same lock the applier uses, so a read never observes a
    /// half-applied command.
    pub fn read<R>(&self, f: impl FnOnce(&Store) -> Result<R>) -> Result<R> {
        let store = self.lock_store()?;
        f(&store)
    }

    /// Propose a mutation and return once it has been applied here.
    pub async fn propose(&self, cmd: Command) -> Result<Applied> {
        if !self.consensus.is_leader() {
            return Err(Error::Unavailable(
                "this member is not the leader; writes must go to the leader".to_string(),
            ));
        }
        let index = self.consensus.commit(&cmd).await?;
        self.apply_committed(index, &cmd)
    }

    /// Apply a committed command. Deterministic: same log, same state, on
    /// every replica.
    ///
    /// Under raft this is called by the applier task walking committed entries
    /// in index order, not by the proposer. It takes `index` for that reason —
    /// so the signature does not have to change when it does.
    pub fn apply_committed(&self, index: u64, cmd: &Command) -> Result<Applied> {
        let applied = {
            let mut store = self.lock_store()?;
            store.apply_at(index, cmd)?
            // The lock is released here, before publishing. Holding it across
            // the fan-out would let one slow watcher block the applier, which
            // is the failure mode this whole design exists to avoid.
        };

        if !applied.events.is_empty() {
            self.watch.publish(applied.revision, applied.events.clone());
        }
        Ok(applied)
    }

    /// Replace the state machine with a snapshot. Only the raft driver calls
    /// this, on a follower that has been told its own state is too far behind
    /// to be caught up from the log.
    pub fn restore_snapshot(&self, state: &crate::store::SnapshotState) -> Result<()> {
        let mut store = self.lock_store()?;
        store.restore_snapshot(state)
    }

    fn lock_store(&self) -> Result<std::sync::MutexGuard<'_, Store>> {
        // A poisoned mutex means a previous apply panicked mid-transaction.
        // sqlite has already rolled that transaction back, so the data is
        // intact, but the process has proven it can panic inside the applier
        // and continuing would be pretending otherwise.
        self.store
            .lock()
            .map_err(|_| Error::Unavailable("the applier panicked; state is unsafe to continue from".to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::{KeyRange, PutOp, RangeQuery};
    use std::path::Path;

    fn node() -> Arc<Node> {
        let store = Store::open(Path::new(":memory:")).unwrap();
        Node::new(store, Box::new(SingleNode::new(1, 1)), 64)
    }

    fn put_cmd(key: &str, value: &str) -> Command {
        Command::Put(PutOp {
            key: key.as_bytes().to_vec(),
            value: value.as_bytes().to_vec(),
            lease: 0,
            prev_kv: false,
            ignore_value: false,
            ignore_lease: false,
        })
    }

    #[tokio::test]
    async fn a_proposal_is_applied_and_readable() {
        let n = node();
        let applied = n.propose(put_cmd("/a", "1")).await.unwrap();
        assert_eq!(applied.revision, 2);
        let got = n
            .read(|s| s.range(&RangeQuery::current(KeyRange::Single(b"/a".to_vec()))))
            .unwrap();
        assert_eq!(got.kvs[0].value, b"1");
    }

    #[tokio::test]
    async fn commit_indices_are_sequential_and_start_at_one() {
        // Not load-bearing on a single node, but raft's are, and this is the
        // counter that becomes the log index.
        let c = SingleNode::new(1, 1);
        assert_eq!(c.commit(&put_cmd("/a", "1")).await.unwrap(), 1);
        assert_eq!(c.commit(&put_cmd("/b", "1")).await.unwrap(), 2);
    }

    #[tokio::test]
    async fn concurrent_proposals_are_serialized_into_distinct_revisions() {
        // The property the mutex is there for: N concurrent writers produce N
        // distinct revisions with no gaps, because apply is the only writer.
        let n = node();
        let mut tasks = Vec::new();
        for i in 0..16 {
            let n = n.clone();
            tasks.push(tokio::spawn(async move {
                n.propose(put_cmd(&format!("/k{i}"), "v")).await.unwrap().revision
            }));
        }
        let mut revisions = Vec::new();
        for t in tasks {
            revisions.push(t.await.unwrap());
        }
        revisions.sort_unstable();
        revisions.dedup();
        assert_eq!(revisions.len(), 16, "every write got its own revision");
        assert_eq!(revisions.first().copied(), Some(2));
        assert_eq!(revisions.last().copied(), Some(17));
    }

    #[tokio::test]
    async fn lease_ids_are_unique_and_positive() {
        // Negative ids are invalid to etcd, and a repeated id would let one
        // client's keepalive refresh another client's lease.
        let n = node();
        let mut ids = std::collections::HashSet::new();
        for _ in 0..1000 {
            let id = n.allocate_lease_id();
            assert!(id > 0, "lease id must be positive, got {id}");
            assert!(ids.insert(id), "duplicate lease id {id}");
        }
    }
}
