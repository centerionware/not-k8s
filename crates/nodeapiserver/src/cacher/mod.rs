//! Group D: the watch cache — an in-memory LIST-then-WATCH snapshot,
//! served instead of always hitting nodestore directly
//! (`docs/APISERVER_PLAN.md` finding 3, `ARCHITECTURE.md` §4).
//!
//! `store` — the cache core (`WatchCache`): apply/list/watch_from/
//! bookmarks/consistent-read waiting. Pure and synchronous underneath, unit
//! tested against synthetic events with no live storage needed.
//!
//! Status: in progress (see docs/APISERVER.md). Landed: the cache core
//! itself. **Not yet landed**: the driver loop that actually runs a
//! `storage::client::StorageClient` LIST + `watch()` against a real
//! nodestore and feeds the results into `WatchCache::apply` (reconnect on
//! disconnect, bookmark generation on a timer), and label/field selector
//! filtering over the cached items — both real, separate work, not
//! implied by the core existing.

pub mod store;

pub use store::{wait_for_revision, CacheEntry, EventKind, Error, WatchCache, WatchEvent};
