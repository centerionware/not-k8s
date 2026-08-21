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
//! — no Rust crate exists to reuse, confirmed, finding 8), the first
//! landed SSA primitive: a real, verified data structure and its exact
//! JSON encoding, not yet the merge algorithm itself.
//!
//! Status: in progress (see docs/APISERVER.md). Landed: RFC 6902/7386,
//! and Strategic Merge Patch's core semantics (recursive object merge,
//! null-deletes-key, merge-by-key for `patch_strategy: merge` lists),
//! and now `fieldset::{PathElement, Set}` — pure, unit-tested against
//! real `fieldsV1` shapes (including the easy-to-miss `"."`-marker case
//! for a path that's both a member and has tracked children). **Not yet
//! landed**: `$patch`/`$setElementOrder`/`$deleteFromPrimitiveList`
//! directives (named, deliberate simplifications — see
//! `strategic_merge`'s own doc comment) and the actual Server-Side Apply
//! merge/conflict-detection algorithm (`typed.mergingWalker` — a much
//! larger, schema-driven undertaking `fieldset` is the first real step
//! toward, not a claim it's done) or any `application/apply-patch+yaml`
//! wiring into `server::rest::patch`.

pub mod fieldset;
pub mod json_patch;
pub mod merge_patch;
pub mod strategic_merge;
