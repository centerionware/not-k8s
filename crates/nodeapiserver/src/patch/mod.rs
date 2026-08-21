//! Group G: JSON Patch, JSON Merge Patch, Strategic Merge Patch, and
//! Server-Side Apply / `managedFields`.
//!
//! `json_patch` — RFC 6902, `merge_patch` — RFC 7386, both thin wrappers
//! around the `json-patch` crate (`docs/APISERVER_PLAN.md` finding 8:
//! reused, not hand-written). `strategic_merge` — the k8s-specific patch
//! kind with no crate to reuse, driven by Group A's `FIELD_META` table
//! (extended with `ref_schema` alongside this module, so recursion has
//! real per-field metadata rather than inheriting the parent's).
//!
//! `fieldset` — Server-Side Apply's own `fieldpath.Set`/`fieldsV1` wire
//! shape (`sigs.k8s.io/structured-merge-diff`, fetched and read directly
//! — no Rust crate exists to reuse, confirmed, finding 8): the pure
//! `PathElement`/`Set` data structure and its exact JSON encoding, plus
//! `set_from_object` — the schema-driven walk (`typed.TypedValue.
//! ToFieldSet()`, ported) that turns one real object into the `Set` of
//! fields it sets, driven by the same Group A `FIELD_META` table
//! `strategic_merge` already reads (its SSA-specific columns:
//! `list_type`/`list_map_keys`/`map_type`, each of the four real
//! decisions confirmed against a real vendored field before writing the
//! code — see `set_from_object`'s own doc comment).
//!
//! Status: in progress (see docs/APISERVER.md). Landed: RFC 6902/7386,
//! Strategic Merge Patch's core semantics (recursive object merge,
//! null-deletes-key, merge-by-key for `patch_strategy: merge` lists),
//! `fieldset::{PathElement, Set}` (pure, unit-tested against real
//! `fieldsV1` shapes including the easy-to-miss `"."`-marker case), and
//! now `fieldset::set_from_object` — one real object in, the `Set` of
//! everything it owns out. **Not yet landed**: `$patch`/
//! `$setElementOrder`/`$deleteFromPrimitiveList` directives (named,
//! deliberate simplifications — see `strategic_merge`'s own doc
//! comment) and the actual Server-Side Apply *merge*/conflict-detection
//! algorithm (`merge.Updater` — combining `set_from_object`'s own output
//! for the incoming request with the existing object's stored
//! `managedFields`, running the real 3-way merge, and rejecting a
//! conflicting field unless `force=true` — a genuinely separate, larger
//! piece `set_from_object` is a real building block toward, not a claim
//! it's done) or any `application/apply-patch+yaml` wiring into
//! `server::rest::patch`.

pub mod fieldset;
pub mod json_patch;
pub mod merge_patch;
pub mod strategic_merge;
