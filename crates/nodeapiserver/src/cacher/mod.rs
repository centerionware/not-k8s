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
//! `selector` — label/field selector parsing and matching, faithful ports
//! of upstream's own `labels`/`fields` package parsers, plus the adapter
//! onto a decoded object (`object_labels`/`field_value`/`object_matches`):
//! labels always live at `metadata.labels` (genuinely generic across every
//! Kind), field lookups are a generic dotted-JSON-path fallback rather
//! than upstream's real per-type `SelectableFields` allowlist (named,
//! not-yet-started work — see that section of the module's own doc
//! comment).
//!
//! Status: in progress (see docs/APISERVER.md). Landed: the cache core,
//! `SharedCache`, the LIST/decode/apply logic, the reconnect loop,
//! bookmark generation, label/field selector parsing+matching, and the
//! adapter onto a decoded `serde_json::Value` object. **Not yet landed**:
//! actually calling this from a real LIST request handler (needs Group
//! E's REST dispatch to exist first) and a per-Kind `SelectableFields`
//! allowlist.

pub mod store;
pub mod driver;
pub mod selector;

pub use store::{wait_for_revision, CacheEntry, EventKind, Error, SharedCache, WatchCache, WatchEvent};
