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
//! `typed_merge` — Server-Side Apply's own real *merge*
//! (`typed.mergingWalker`, ported): combines a live object with an
//! incoming apply configuration into the merged result, reading the
//! same SSA-specific `FIELD_META` columns `fieldset::set_from_object`
//! does. A real, deliberate sibling of `strategic_merge`, not a
//! duplicate — the two differ in two confirmed, real ways
//! (`list_type: "set"` real deduplicated-union merging, which SMP has
//! no equivalent concept of at all; `map_type: "atomic"` wholesale
//! replacement, which SMP's own "every object field merges recursively"
//! default has no exception for) — see `typed_merge`'s own doc comment
//! for the full comparison.
//!
//! `typed_compare` — Server-Side Apply's own real *diff*
//! (`typed.TypedValue.Compare`, ported): given a "before" and "after"
//! object, produces the `Comparison{removed, modified, added}` triple
//! of `Set`s `Updater.Apply` needs to know which fields an apply
//! actually *changed* (not just which fields it mentions —
//! `typed_merge::merge` already answers that structurally), the last of
//! `merge.Updater`'s own three real prerequisites to land before the
//! orchestration itself.
//!
//! Status: in progress (see docs/APISERVER.md). Landed: RFC 6902/7386,
//! Strategic Merge Patch's core semantics (recursive object merge,
//! null-deletes-key, merge-by-key for `patch_strategy: merge` lists),
//! `fieldset::{PathElement, Set}` (pure, unit-tested against real
//! `fieldsV1` shapes including the easy-to-miss `"."`-marker case),
//! `fieldset::set_from_object` (one real object in, the `Set` of
//! everything it owns out), `Set`'s own algebra
//! (`union`/`intersection`/`difference`/`recursive_difference`/
//! `is_empty` — real upstream's own `Set`/`SetNodeMap`/`PathElementSet`
//! methods; `difference`'s own doc comment names a real, intentional
//! asymmetry with `recursive_difference` a naive from-memory port would
//! likely miss), `typed_merge::merge` (the real merged *value* two typed
//! objects combine into), and now `typed_compare::compare` (the real
//! `{removed, modified, added}` diff between two typed objects — a
//! field present on only one side is inserted *both* at its own path
//! and recursively at every leaf beneath it, real upstream's own
//! genuinely non-redundant rule, confirmed directly against
//! `compareWalker.compare`'s own source rather than assumed from the
//! method name alone). **Not yet landed**: `$patch`/`$setElementOrder`/
//! `$deleteFromPrimitiveList` directives (named, deliberate
//! simplifications — see `strategic_merge`'s own doc comment) and
//! `Updater.Apply` itself: the orchestration that combines `typed_merge`/
//! `typed_compare`'s own output with the `Set` algebra to do real
//! conflict detection against other managers' stored `managedFields`
//! and reject a conflicting field unless `force=true` — the piece that
//! actually closes this arc, all three of its real prerequisites now
//! landed but the orchestration itself not yet started, nor is any
//! `application/apply-patch+yaml` wiring into `server::rest::patch`.

pub mod fieldset;
pub mod json_patch;
pub mod merge_patch;
pub mod strategic_merge;
pub mod typed_compare;
pub mod typed_merge;
