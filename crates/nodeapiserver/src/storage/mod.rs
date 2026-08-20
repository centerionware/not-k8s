//! Group C: the etcd v3 client to nodestore.
//!
//! `pb` — the generated client types (`build.rs` compiles `proto/rpc.proto`,
//! a synced copy of nodestore's own already-vendored protos —
//! `proto/sync-from-nodestore.sh`).
//! `client` — `StorageClient`, wrapping `Range`/`Put`/`DeleteRange`/`Txn`/`Watch`.
//! `keys` — the etcd key layout, **unconfigured-default only** — see its
//! own module doc for the real override table this doesn't have yet.
//!
//! Status: in progress (see docs/APISERVER.md). Landed: the gRPC client
//! with the same mutual-TLS posture nodestore's own client API requires
//! (including the bidirectional `Watch` RPC and the standard
//! prefix->range-end helper), and the default (no-override) key layout.
//! `resourceVersion == nodestore's MVCC revision` (finding 3) and
//! optimistic-concurrency-via-`Txn` are real today. **Not yet landed, and
//! deliberately not guessed at**: the per-resource key-prefix override
//! table (`keys`'s own doc comment) and encryption-at-rest providers.
//! `Lease` RPCs are a follow-up, added when a lease-backed subresource
//! needs them.

pub mod pb;
pub mod client;
pub mod keys;

pub use client::{Error, StorageClient, WatchHandle};
