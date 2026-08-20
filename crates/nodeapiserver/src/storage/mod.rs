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
//! `encryption_config` — parses a real `EncryptionConfiguration` YAML
//! document into a resolvable, per-resource set of `encryption`
//! transformers (`aesgcm`/`identity` only, matching `encryption`'s own
//! scope; `aescbc`/`secretbox`/`kms` parse structurally but resolve to a
//! real, named error rather than being silently dropped) — see that
//! module's own doc comment for the real resource-name/wildcard matching
//! rules ported.
//!
//! Status: in progress (see docs/APISERVER.md). The gRPC client has the
//! same mutual-TLS posture nodestore's own client API requires (`Watch`
//! and `LeaseKeepAlive`'s bidirectional streams included, plus the
//! standard prefix->range-end helper), and the key layout matches real
//! upstream exactly, override table included. `resourceVersion ==
//! nodestore's MVCC revision` (finding 3) and
//! optimistic-concurrency-via-`Txn` are real. Encryption-at-rest
//! transform primitives (`Identity`, AES-256-GCM) and their
//! `EncryptionConfiguration` YAML config loader both now exist, but
//! neither is **wired into `StorageClient`'s own read/write path or
//! `cacher`'s `Watch` decoding** yet — a real, separate, deliberately
//! not-yet-started piece of work (see `encryption_config`'s own doc
//! comment for why transparent encryption needs every one of
//! `range`/`put`/`txn`/`watch` to agree before it's safe to turn on at
//! all, not just some of them).

pub mod pb;
pub mod client;
pub mod keys;
pub mod encryption;
pub mod encryption_config;

pub use client::{Error, LeaseKeepAliveHandle, StorageClient, WatchHandle};
