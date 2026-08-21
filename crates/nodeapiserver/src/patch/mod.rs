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
//! # Server-Side Apply — the full real orchestration, now landed
//!
//! `fieldset` — the pure `PathElement`/`Set` data structure and its exact
//! `fieldsV1` JSON encoding (`sigs.k8s.io/structured-merge-diff`, fetched
//! and read directly — no Rust crate exists to reuse, confirmed, finding
//! 8), `Set`'s own algebra (`union`/`intersection`/`difference`/
//! `recursive_difference`/`is_empty` — `difference`'s own doc comment
//! names a real, intentional asymmetry with `recursive_difference` a
//! naive from-memory port would likely miss), `set_from_object` (one real
//! object in, the `Set` of everything it owns out — `typed.TypedValue.
//! ToFieldSet()`, ported), `remove_items` (the inverse — `TypedValue.
//! RemoveItems`, ported), and `ensure_named_fields_are_members` (real
//! `Set.EnsureNamedFieldsAreMembers`, needed so a formerly wholly-owned
//! struct field tracked only via its leaves can still be matched and
//! pruned as a whole). All four schema-driven functions read the same
//! Group A `FIELD_META` columns (`list_type`/`list_map_keys`/`map_type`/
//! `ref_schema`).
//!
//! `typed_merge` — the real *merge* (`typed.mergingWalker`, ported): a
//! real, deliberate sibling of `strategic_merge`, not a duplicate — they
//! differ in two confirmed, real ways (`list_type: "set"` deduplicated
//! union, which SMP has no equivalent of; `map_type: "atomic"` wholesale
//! replacement, which SMP's own "every object field merges recursively"
//! default has no exception for) — see `typed_merge`'s own doc comment.
//!
//! `typed_compare` — the real *diff* (`typed.TypedValue.Compare`,
//! ported): a field present on only one side is inserted *both* at its
//! own path and recursively at every leaf beneath it, real upstream's
//! own genuinely non-redundant rule, confirmed directly against
//! `compareWalker.compare`'s own source.
//!
//! `updater` — `merge.Updater` itself: `update()` (conflict-detection/
//! bookkeeping core), `apply_update()` (`Updater.Update`, the PATCH/PUT
//! path), `prune()` (`Updater.prune`/`addBackOwnedItems`/
//! `addBackDanglingItems`), and `apply()` (`Updater.Apply` itself — the
//! real orchestration this whole arc has been building toward: merge the
//! config in, record the applying manager's new `Set`, prune what it
//! stopped claiming, run real conflict detection against every other
//! manager). Single-schema-version scoped throughout (see `updater`'s own
//! module doc): this build has no multi-version conversion machinery at
//! all, so every place real upstream caches one `Comparison`/converts an
//! object per served API version collapses to one shared computation.
//!
//! `managed_fields` — the real `metadata.managedFields[]` wire shape
//! (`ManagedFieldsEntry`, confirmed directly against the vendored
//! OpenAPI spec) and the two conversions `updater`'s functions actually
//! need: the stored array in, a `BTreeMap<String, Set>` out; a
//! reconciled map plus the applying manager's own new entry in, the
//! array to write back out.
//!
//! **Server-Side Apply is now reachable from a real request**:
//! `server::rest::apply_patch` wires `updater::apply` + `managed_fields`
//! to real storage, and `server::listener` routes `PATCH` with
//! `Content-Type: application/apply-patch+yaml` (`?fieldManager=`
//! required, `?force=true` honored) into it, with a real `409 Conflict`
//! on `Err(Vec<Conflict>)`. **Named, honest scope for this first wiring
//! slice** (`rest::apply_patch`'s own doc comment): only against an
//! **already-existing** object (no create-on-apply yet — `updater::apply`
//! itself already supports it structurally, only the storage-side
//! create-if-absent `Txn` isn't wired), and only for a built-in resource
//! (not yet supported for a CRD-defined one, since `updater::apply`'s
//! primitives key off compiled `FIELD_META`, not a runtime CRD schema).
//! Only `namespace_lifecycle` admission runs on this path, not
//! `LimitRanger`'s PVC check the ordinary three-patch-kind branch also
//! runs — real, separate follow-up.
//!
//! **Not yet landed**: `$patch`/`$setElementOrder`/
//! `$deleteFromPrimitiveList` directives (named, deliberate
//! simplifications — see `strategic_merge`'s own doc comment), plus the
//! two gaps named above (create-on-apply, CRD support).

pub mod fieldset;
pub mod json_patch;
pub mod managed_fields;
pub mod merge_patch;
pub mod strategic_merge;
pub mod typed_compare;
pub mod typed_merge;
pub mod updater;
