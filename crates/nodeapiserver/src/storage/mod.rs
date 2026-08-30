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
//! AES-256-CBC, and the generic prefix-dispatch composition every provider
//! list uses),
//! a faithful port of a real subset of upstream's `storage/value` package
//! — see that module's own doc comment for exactly which providers are
//! and aren't covered.
//! `encryption_config` — parses a real `EncryptionConfiguration` YAML
//! document into a resolvable, per-resource set of `encryption`
//! transformers (`aesgcm`/`aescbc`/`identity`; `secretbox`/`kms` parse
//! structurally but resolve to a real, named error rather than being
//! silently dropped) — see that
//! module's own doc comment for the real resource-name/wildcard matching
//! rules ported.
//!
//! Status: in progress (see docs/APISERVER.md). The gRPC client has the
//! same mutual-TLS posture nodestore's own client API requires (`Watch`
//! and `LeaseKeepAlive`'s bidirectional streams included, plus the
//! standard prefix->range-end helper), and the key layout matches real
//! upstream exactly, override table included. `resourceVersion ==
//! nodestore's MVCC revision` (finding 3) and
//! optimistic-concurrency-via-`Txn` are real. **Encryption-at-rest is
//! wired end to end now** — `range`/`put`/`txn`/`watch` all agree
//! (`StorageClient::with_encryption` attaches the loaded config,
//! `server::rest::decrypt_and_decode`/`encrypt_for_storage` are the
//! entire wiring surface, one shared pair every real verb and `watch`
//! funnels through — see `docs/APISERVER.md`'s own Group C section for
//! the full account).

pub mod pb;
pub mod client;
pub mod keys;
pub mod encryption;
pub mod encryption_config;

pub use client::{Error, LeaseKeepAliveHandle, StorageClient, WatchHandle};
