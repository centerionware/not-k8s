//! Group D: the watch cache — an in-memory LIST-then-WATCH snapshot,
//! served instead of always hitting nodestore directly
//! (`docs/APISERVER_PLAN.md` finding 3, `ARCHITECTURE.md` §4).
//!
//! `store` — the cache core (`WatchCache`): apply/list/get/watch_from/
//! bookmarks/consistent-read waiting (`get` is `list`'s single-key
//! equivalent, for a `GET` rather than a `LIST`), plus `SharedCache` (an
//! `Arc<RwLock<..>>` wrapper — what a driver loop and its readers
//! actually hold, since both sides need concurrent access to the same
//! cache). Pure and synchronous underneath, unit tested against
//! synthetic events with no live storage needed.
//! `driver` — wires a real `storage::client::StorageClient` to the core:
//! LIST for a snapshot + RV, WATCH from `RV + 1`, decode `mvccpb::Event`s
//! into `WatchCache::apply` calls, and `reflect()` — the reconnect loop
//! that runs this forever, relisting on any failure or stream end, and
//! periodically sending an explicit `WatchProgressRequest` to generate a
//! bookmark (nodestore's own server never generates one unprompted —
//! confirmed by reading its watch handler, not assumed).
//! `selector` — label/field selector parsing and matching, faithful ports
//! of upstream's own `labels`/`fields` package parsers, plus the adapter
//! onto a decoded object (`object_labels`/`field_value`/`object_matches`):
//! labels always live at `metadata.labels` (genuinely generic across every
//! Kind), field lookups are a generic dotted-JSON-path fallback rather
//! than upstream's real per-type `SelectableFields` allowlist (named,
//! not-yet-started work — see that section of the module's own doc
//! comment).
//! `registry` — starts a `driver::reflect()` background loop for one
//! resource and hands back the `SharedCache` it keeps live
//! (`CacheRegistry::spawn`). The primitive only — it does not yet
//! enumerate every resource this build knows about and start one for
//! each at boot (a real integration decision, not built) — see that
//! module's own doc comment for exactly what's deferred and why.
//!
//! Status: in progress (see docs/APISERVER.md). Landed: the cache core,
//! `SharedCache` (including `has_synced()` — real `client-go`
//! `HasSynced()`, ported: a fresh cache starts unsynced, the first
//! completed `replace()` — the reconnect loop's own first `LIST` — marks
//! it permanently, so a reader can tell "registered but still mid-relist"
//! apart from "synced, genuinely empty" without guessing from the
//! revision), the LIST/decode/apply logic, the reconnect loop, bookmark
//! generation, label/field selector parsing+matching, the adapter onto a
//! decoded `serde_json::Value` object (`object_matches` is called for
//! real, from `server::rest::list`, Group E), a single-resource
//! cache-registration primitive (`registry::CacheRegistry`), and
//! `server::rest::get`/`list` both now consulting a cache when one is
//! passed in (`get`: a hit skips nodestore, a miss always falls through;
//! `list`: only once `has_synced()`, for the reason above — see `rest`'s
//! own doc comment for the full contract of each).
//! **Not yet landed**: a per-Kind `SelectableFields` allowlist, and
//! registering a cache for *every* resource at boot — today
//! `server::listener`'s own `BOOT_CACHED_RESOURCES` (a deliberately
//! bounded, reasoned list, not every resource) is what passes
//! `Some(cache)` to `get`/`list`; every resource outside that list still
//! passes `None`, same as before caching existed for it.

pub mod store;
pub mod driver;
pub mod selector;
pub mod registry;

pub use store::{wait_for_revision, CacheEntry, EventKind, Error, SharedCache, WatchCache, WatchEvent};
pub use registry::CacheRegistry;
