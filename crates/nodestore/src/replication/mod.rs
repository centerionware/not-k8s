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
//! Still to come on this branch: the driver (the Ready loop), the peer
//! transport, and snapshot build/restore. The pieces already here are the
//! ones everything else stands on, and they are tested on their own.

pub mod log;
pub mod logging;
