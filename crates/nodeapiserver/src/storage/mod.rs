//! Group C: the etcd v3 client to nodestore.
//!
//! `pb` — the generated client types (`build.rs` compiles `proto/rpc.proto`,
//! a synced copy of nodestore's own already-vendored protos —
//! `proto/sync-from-nodestore.sh`).
//! `client` — `StorageClient`, wrapping `Range`/`Put`/`DeleteRange`/`Txn`/
//! `Watch`/`Lease*`.
//! `keys` — the etcd key layout, including the real per-resource prefix
//! override table (`SpecialDefaultResourcePrefixes`).
//! `encryption` — encryption-at-rest transformers (`Identity`, AES-256-GCM,
//! and the generic prefix-dispatch composition every provider list uses),
//! a faithful port of a real subset of upstream's `storage/value` package
//! — see that module's own doc comment for exactly which providers are
//! and aren't covered.
//!
//! Status: in progress (see docs/APISERVER.md). The gRPC client has the
//! same mutual-TLS posture nodestore's own client API requires (`Watch`
//! and `LeaseKeepAlive`'s bidirectional streams included, plus the
//! standard prefix->range-end helper), and the key layout matches real
//! upstream exactly, override table included. `resourceVersion ==
//! nodestore's MVCC revision` (finding 3) and
//! optimistic-concurrency-via-`Txn` are real. Encryption-at-rest
//! transform primitives now exist (`Identity`, AES-256-GCM) but are
//! **not yet wired into `StorageClient`'s own read/write path**, and
//! `EncryptionConfiguration` YAML parsing (which provider(s) apply to
//! which resource) isn't built — both real, separate, not-yet-started
//! work.

pub mod pb;
pub mod client;
pub mod keys;
pub mod encryption;

pub use client::{Error, LeaseKeepAliveHandle, StorageClient, WatchHandle};
