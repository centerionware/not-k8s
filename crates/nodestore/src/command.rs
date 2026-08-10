//! The state machine's input language — and, when raft lands, the raft log
//! entry type.
//!
//! # Why these types exist at all, instead of just using the generated protos
//!
//! Two different contracts are being kept, and conflating them is the trap:
//!
//!   * The **wire** contract (`proto/rpc.proto`) is etcd's, frozen by the fact
//!     that a real kube-apiserver speaks it. It changes when etcd changes.
//!   * The **log** contract is ours. Under raft, a committed entry is replayed
//!     on every replica and after every restart, possibly by a *newer* binary
//!     than the one that wrote it. Its encoding is therefore a compatibility
//!     surface with our own past, and it must not move because etcd bumped a
//!     field number or grpc regenerated a struct.
//!
//! So the server layer translates wire types into these, and the state machine
//! only ever sees these.
//!
//! # Determinism is the whole design
//!
//! Every replica applies the same commands in the same order and must reach
//! byte-identical state. That rules out reading anything ambient during
//! `apply`: no clock, no RNG, no environment. Where a mutation genuinely
//! depends on such a thing, the *leader* resolves it before proposing and
//! stamps the answer into the command — which is why [`Command::LeaseGrant`]
//! carries an `id` the leader picked rather than letting each replica invent
//! one, and why lease expiry is [`Command::ExpireLeases`] with an explicit
//! `now_unix_secs` rather than each replica consulting its own clock.
//!
//! This is enforced by construction today, on a single node where it cannot
//! yet be observed to matter. Getting it wrong now would mean discovering it
//! later as replicas silently diverging, which is the single worst failure
//! mode a datastore has.

use serde::{Deserialize, Serialize};

/// A key range, in etcd's own encoding.
///
/// etcd overloads `range_end` in ways that are easy to get subtly wrong, so it
/// is decoded exactly once, here, rather than at each call site:
///
///   * empty            — the single key `key`
///   * `[0x00]`         — every key `>= key`; with `key` also `[0x00]`, every
///                        key in the store (this is how "list everything" is
///                        spelled on the wire)
///   * anything else    — the half-open interval `[key, range_end)`
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum KeyRange {
    Single(Vec<u8>),
    /// `[from, to)`.
    Between { from: Vec<u8>, to: Vec<u8> },
    /// `[from, ∞)`.
    From(Vec<u8>),
    /// Every key.
    All,
}

impl KeyRange {
    /// Decode a wire `(key, range_end)` pair.
    pub fn decode(key: Vec<u8>, range_end: Vec<u8>) -> Self {
        match (key.as_slice(), range_end.as_slice()) {
            (_, []) => KeyRange::Single(key),
            ([0], [0]) => KeyRange::All,
            (_, [0]) => KeyRange::From(key),
            _ => KeyRange::Between { from: key, to: range_end },
        }
    }

    /// Whether a key falls in this range — the same predicate the SQL below
    /// expresses, kept here so watchers can filter live events without a
    /// round trip to the database.
    pub fn contains(&self, key: &[u8]) -> bool {
        match self {
            KeyRange::Single(k) => key == k.as_slice(),
            KeyRange::All => true,
            KeyRange::From(from) => key >= from.as_slice(),
            KeyRange::Between { from, to } => key >= from.as_slice() && key < to.as_slice(),
        }
    }
}

/// One mutation, as recorded in the log.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Command {
    Put(PutOp),
    Delete(DeleteOp),
    Txn(TxnOp),
    /// Discard history at or below `revision`. `revision` is resolved by the
    /// leader before proposing (etcd allows a *relative* compaction request,
    /// which must not be re-resolved per replica).
    Compact { revision: i64 },
    /// `id` *and* `now_unix_secs` are both chosen by the leader; see the
    /// determinism note above. The clock reading is in the command because a
    /// lease's expiry must be identical on every replica, and each replica
    /// reading its own clock at apply time would not be.
    LeaseGrant { id: i64, ttl_secs: i64, now_unix_secs: i64 },
    LeaseRevoke { id: i64 },
    LeaseKeepAlive { id: i64, now_unix_secs: i64 },
    /// Delete every key held by a lease that has expired as of
    /// `now_unix_secs`. Proposed by the leader's expiry loop; the timestamp is
    /// in the command precisely so replicas don't each read their own clock.
    ExpireLeases { now_unix_secs: i64 },

    /// Record how to reach a member.
    ///
    /// raft knows member *ids* and nothing else; addresses are our problem.
    /// Replicating them through the log — rather than keeping them in each
    /// node's own configuration — is what lets a node that joins later, or
    /// restarts, learn the cluster's shape from the state it already has to
    /// catch up on, instead of from whatever its operator remembered to
    /// configure. It is also what makes forwarding possible: a follower has
    /// to know the leader's client URL, and the leader is identified to it by
    /// id.
    SetMember(Member),

    /// Forget a member. Proposed alongside the raft configuration change that
    /// removes it, so the address book cannot outlive the membership.
    RemoveMember { id: u64 },
}

/// A cluster member, as recorded in the replicated address book.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Member {
    pub id: u64,
    /// Where peers reach this member for raft traffic.
    pub peer_url: String,
    /// Where clients — and forwarding followers — reach its etcd API.
    pub client_url: String,
    pub name: String,
    /// A learner receives the log but does not vote and is not counted for
    /// quorum. New members join this way so that catching up a fresh replica
    /// cannot stall the cluster it is joining.
    pub is_learner: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PutOp {
    pub key: Vec<u8>,
    pub value: Vec<u8>,
    pub lease: i64,
    pub prev_kv: bool,
    /// Update only the lease, keeping the existing value (etcd's
    /// `ignore_value`). Fails if the key doesn't exist.
    pub ignore_value: bool,
    /// Update only the value, keeping the existing lease.
    pub ignore_lease: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeleteOp {
    pub range: KeyRange,
    pub prev_kv: bool,
}

/// A compare-and-swap transaction: if every comparison holds, apply `success`,
/// otherwise apply `failure`. Either branch may be empty.
///
/// This is the only operation kube-apiserver uses for writes — every update it
/// makes is `compare mod_revision == the resourceVersion I read` guarding a
/// put, which is exactly how optimistic concurrency reaches the storage layer.
/// If this is wrong, two clients racing on one object both win.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TxnOp {
    pub compare: Vec<Compare>,
    pub success: Vec<RequestOp>,
    pub failure: Vec<RequestOp>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Compare {
    pub key: Vec<u8>,
    pub result: CompareResult,
    pub target: CompareTarget,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum CompareResult {
    Equal,
    Greater,
    Less,
    NotEqual,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum CompareTarget {
    Version(i64),
    CreateRevision(i64),
    ModRevision(i64),
    Value(Vec<u8>),
    Lease(i64),
}

/// An operation inside a transaction branch. Nested transactions are part of
/// the etcd API; apiserver doesn't use them, but a `Range` in the failure
/// branch is how it reads back the value it lost a race to.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum RequestOp {
    Range(RangeQuery),
    Put(PutOp),
    Delete(DeleteOp),
}

/// A read. Not a [`Command`] — reads never enter the log — but it shares this
/// module because a transaction branch can contain one.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RangeQuery {
    pub range: KeyRange,
    /// Read the store as it was at this revision; 0 means "as it is now".
    pub revision: i64,
    pub limit: i64,
    pub keys_only: bool,
    pub count_only: bool,
    pub sort: Option<Sort>,
}

impl RangeQuery {
    /// A plain current-revision read of `range`, which is most of them.
    pub fn current(range: KeyRange) -> Self {
        RangeQuery { range, revision: 0, limit: 0, keys_only: false, count_only: false, sort: None }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Sort {
    pub target: SortTarget,
    pub ascending: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SortTarget {
    Key,
    Version,
    CreateRevision,
    ModRevision,
    Value,
}

#[cfg(test)]
mod tests {
    use super::*;

    // range_end's overloading is the part of the etcd API most likely to be
    // implemented by guessing. These are the four cases apiserver actually
    // produces: a single-key get, a prefix list, an open-ended scan, and the
    // "everything" form used on the initial list.
    #[test]
    fn decodes_a_single_key() {
        assert_eq!(KeyRange::decode(b"/a".to_vec(), vec![]), KeyRange::Single(b"/a".to_vec()));
    }

    #[test]
    fn decodes_a_prefix_as_a_half_open_interval() {
        let r = KeyRange::decode(b"/registry/pods/".to_vec(), b"/registry/pods0".to_vec());
        assert!(r.contains(b"/registry/pods/default/nginx"));
        assert!(!r.contains(b"/registry/pods0"), "range_end is exclusive");
        assert!(!r.contains(b"/registry/nodes/n1"));
    }

    #[test]
    fn decodes_the_open_ended_form() {
        let r = KeyRange::decode(b"/m".to_vec(), vec![0]);
        assert_eq!(r, KeyRange::From(b"/m".to_vec()));
        assert!(r.contains(b"/z"));
        assert!(!r.contains(b"/a"));
    }

    #[test]
    fn decodes_the_everything_form() {
        let r = KeyRange::decode(vec![0], vec![0]);
        assert_eq!(r, KeyRange::All);
        assert!(r.contains(b""));
        assert!(r.contains(b"\xff\xff"));
    }

    // A zero byte is a legal key byte, so `key = [0]` with a real range_end
    // must NOT be mistaken for the "everything" form.
    #[test]
    fn a_zero_byte_key_with_a_real_end_is_an_ordinary_interval() {
        let r = KeyRange::decode(vec![0], b"/z".to_vec());
        assert_eq!(r, KeyRange::Between { from: vec![0], to: b"/z".to_vec() });
    }

    #[test]
    fn single_key_ranges_match_only_that_key() {
        let r = KeyRange::Single(b"/a".to_vec());
        assert!(r.contains(b"/a"));
        assert!(!r.contains(b"/a/b"));
        assert!(!r.contains(b"/"));
    }
}
