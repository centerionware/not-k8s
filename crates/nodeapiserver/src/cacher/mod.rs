//! Group D: the watch cache — an in-memory LIST-then-WATCH snapshot,
//! served instead of always hitting nodestore directly
//! (`docs/APISERVER_PLAN.md` finding 3, `ARCHITECTURE.md` §4).
//!
//! `store` — the cache core (`WatchCache`): apply/list/watch_from/
//! bookmarks/consistent-read waiting, plus `SharedCache` (an `Arc<RwLock<..>>`
//! wrapper — what a driver loop and its readers actually hold, since both
//! sides need concurrent access to the same cache). Pure and synchronous
//! underneath, unit tested against synthetic events with no live storage
//! needed.
//! `driver` — wires a real `storage::client::StorageClient` to the core:
//! LIST for a snapshot + RV, WATCH from `RV + 1`, decode `mvccpb::Event`s
//! into `WatchCache::apply` calls, and `reflect()` — the reconnect loop
//! that runs this forever, relisting on any failure or stream end, and
//! periodically sending an explicit `WatchProgressRequest` to generate a
//! bookmark (nodestore's own server never generates one unprompted —
//! confirmed by reading its watch handler, not assumed).
//!
//! Status: in progress (see docs/APISERVER.md). Landed: the cache core,
//! `SharedCache`, the LIST/decode/apply logic, the reconnect loop, and
//! bookmark generation. **Not yet landed**: label/field selector filtering
//! over the cached items.

pub mod store;
pub mod driver;

pub use store::{wait_for_revision, CacheEntry, EventKind, Error, SharedCache, WatchCache, WatchEvent};
