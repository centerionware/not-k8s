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
//! Status: in progress (see docs/APISERVER.md). Landed: RFC 6902/7386,
//! and Strategic Merge Patch's core semantics (recursive object merge,
//! null-deletes-key, merge-by-key for `patch_strategy: merge` lists).
//! **Not yet landed**: `$patch`/`$setElementOrder`/
//! `$deleteFromPrimitiveList` directives (named, deliberate simplifications
//! — see `strategic_merge`'s own doc comment) and Server-Side Apply/
//! `managedFields` (structured-merge-diff has no Rust crate to reuse
//! either, per finding 8).

pub mod json_patch;
pub mod merge_patch;
pub mod strategic_merge;
