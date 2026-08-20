//! Group D: the watch cache — an in-memory LIST-then-WATCH snapshot,
//! served instead of always hitting nodestore directly
//! (`docs/APISERVER_PLAN.md` finding 3, `ARCHITECTURE.md` §4).
//!
//! `store` — the cache core (`WatchCache`): apply/list/watch_from/
//! bookmarks/consistent-read waiting. Pure and synchronous underneath, unit
//! tested against synthetic events with no live storage needed.
//! `driver` — wires a real `storage::client::StorageClient` to the core:
//! LIST for a snapshot + RV, WATCH from `RV + 1`, decode `mvccpb::Event`s
//! into `WatchCache::apply` calls. Decode logic is pure/unit-tested; the
//! two functions that actually call `StorageClient` are thin wrappers.
//!
//! Status: in progress (see docs/APISERVER.md). Landed: the cache core,
//! and the LIST/decode/apply logic to drive it from real nodestore
//! responses. **Not yet landed**, named honestly rather than implied by
//! the above: a reconnect-on-disconnect loop (today's driver functions
//! give a caller one LIST-then-WATCH cycle; noticing the stream end and
//! starting over is still the caller's job), bookmark *generation* on a
//! timer (nodestore's `progress_notify` mechanism would drive this — this
//! module only turns a progress-notify response into a cache bookmark, it
//! doesn't request one on any schedule), and label/field selector
//! filtering over the cached items.

pub mod store;
pub mod driver;

pub use store::{wait_for_revision, CacheEntry, EventKind, Error, WatchCache, WatchEvent};
