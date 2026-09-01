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
//! Kind), and field selectors are checked against the built-in per-Kind
//! `SelectableFields` allowlist before the generic dotted-JSON-path lookup
//! runs. CRDs accept universal metadata fields plus their served version's
//! declared `spec.selectableFields`.
//! `registry` — starts or stops a `driver::reflect()` background loop for
//! one resource and hands back the `SharedCache` it keeps live
//! (`CacheRegistry::spawn`/`remove`). The listener invokes it for every
//! built-in resource at boot and reconciles CRD-defined resources from the
//! live CRD cache.
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
//! cache-registration primitive (`registry::CacheRegistry`), including
//! CRD lifecycle registration/teardown, and
//! `server::rest::get`/`list` both now consulting a cache when one is
//! passed in (`get`: a hit skips nodestore, a miss always falls through;
//! `list`: only once `has_synced()`, for the reason above — see `rest`'s
//! own doc comment for the full contract of each).
//! **Per-Kind `SelectableFields` is now enforced**: built-in resources use
//! their verified metadata and resource-specific fields, while CRDs accept
//! only universal metadata fields; unsupported paths are rejected rather than
//! silently matching no objects. Remaining work is cache compatibility
//! hardening, not basic registration: built-in resources are registered at
//! boot and CRD resources follow their live lifecycle.

pub mod store;
pub mod driver;
pub mod selector;
pub mod registry;

pub use store::{wait_for_revision, CacheEntry, EventKind, Error, SharedCache, WatchCache, WatchEvent};
pub use registry::CacheRegistry;
