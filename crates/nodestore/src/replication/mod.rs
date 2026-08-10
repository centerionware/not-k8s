//! Replication: raft consensus over the deterministic state machine.
//!
//! Named `replication` rather than `raft` on purpose — a module called `raft`
//! sitting next to the `raft` crate makes every `use raft::…` in this crate
//! ambiguous, and disambiguating each one with `::raft::` is a tax paid
//! forever for a name we don't need.
//!
//! What lives here:
//!
//!   * [`log`] — the raft log in sqlite (`raft::Storage`), plus the durability
//!     argument for why it is a *separate* database from the state machine's.
//!   * `logging` — bridges raft-rs's slog output into tracing, so an election
//!     or a leadership change lands in the same log as everything else rather
//!     than vanishing.
//!
//!   * [`transport`] — raft messages over our own gRPC, deliberately lossy
//!     because that is the interface raft is designed against.
//!   * [`proposals`] — routing an applied result back to the caller that
//!     proposed it, and failing the ones a lost leadership invalidated.
//!   * [`driver`] — the Ready loop. One task owns the RawNode, because the
//!     order its steps happen in is part of raft's correctness rather than a
//!     style choice.
//!
//! Still to come on this branch: wiring the driver into `Consensus` so the
//! gRPC layer proposes through it, forwarding writes from a follower to the
//! leader, and the netns-based failover e2e.

pub mod driver;
pub mod log;
pub mod logging;
pub mod proposals;
pub mod transport;
