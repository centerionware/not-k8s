//! Group C: the etcd v3 client to nodestore.
//!
//! `pb` — the generated client types (`build.rs` compiles `proto/rpc.proto`,
//! a synced copy of nodestore's own already-vendored protos —
//! `proto/sync-from-nodestore.sh`).
//! `client` — `StorageClient`, wrapping `Range`/`Put`/`DeleteRange`/`Txn`/
//! `Watch`/`Lease*`.
//! `keys` — the etcd key layout, including the real per-resource prefix
//! override table (`SpecialDefaultResourcePrefixes`).
//!
//! Status: in progress (see docs/APISERVER.md). The gRPC client has the
//! same mutual-TLS posture nodestore's own client API requires (`Watch`
//! and `LeaseKeepAlive`'s bidirectional streams included, plus the
//! standard prefix->range-end helper), and the key layout matches real
//! upstream exactly, override table included. `resourceVersion ==
//! nodestore's MVCC revision` (finding 3) and
//! optimistic-concurrency-via-`Txn` are real. **Not yet landed**:
//! encryption-at-rest providers — the one item left on Group C's own
//! plan-file scope (`docs/APISERVER_PLAN.md`).

pub mod pb;
pub mod client;
pub mod keys;

pub use client::{Error, LeaseKeepAliveHandle, StorageClient, WatchHandle};
