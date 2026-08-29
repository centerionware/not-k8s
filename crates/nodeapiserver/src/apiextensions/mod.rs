//! Group K: CustomResourceDefinitions.
//!
//! `registry` — dynamic `(group, version, resource)` resolution against
//! stored, `Established` CRDs, `server::rest`'s fallback once its own
//! static `resolve_kind` table misses. `conditions` — the
//! `NamesAccepted`/`Established`/`storedVersions` status this build
//! computes synchronously on `CREATE`/`UPDATE` of a CRD itself (this
//! build's own stand-in for real upstream's separate async establishing
//! controller — see that module's own doc comment for why). `schema_defaults`
//! — structural-schema defaulting for a CRD-defined object's own body,
//! walking `spec.versions[].schema.openAPIV3Schema` at runtime (there's
//! no compiled `FIELD_META` for an arbitrary operator-defined schema the
//! way built-in types get from Group A's codegen).
//!
//! `schema_validation` — the required/type-checking sibling of
//! `schema_defaults`, same runtime-schema walk, producing
//! `scheme::validation`'s own `MissingField`/`TypeMismatch` shapes so
//! every real call site already knows how to report either kind
//! identically. `schema_pruning` — drops any object key the schema
//! doesn't declare (unless `x-kubernetes-preserve-unknown-fields` opts a
//! subtree out), real upstream's own default posture for a structural
//! (`apiextensions.k8s.io/v1`) schema; `apiVersion`/`kind`/`metadata` are
//! hard-coded as always preserved at the object's own top level, standing
//! in for real upstream's schema-completion step that auto-injects them.
//! Schema-local enum, range, length, pattern, uniqueness, and standard
//! scalar-format constraints are enforced by `schema_validation` too;
//! cross-field consistency remains the CRD's CEL responsibility.
//!
//! `schema_strategic_merge` — the runtime-schema sibling of
//! `crate::patch::strategic_merge`: a list field merges by key when its
//! own schema names `x-kubernetes-list-type: map` +
//! `x-kubernetes-list-map-keys` (a real array — composite keys included,
//! genuinely more than the compiled path's single `patch_merge_key`
//! since built-in types never need more than one, not a reason to cap
//! the CRD path the same way).
//!
//! `schema_apply` — runtime-schema Server-Side Apply for CRD-defined
//! objects. It produces the same `fieldsV1` field sets as built-in SSA,
//! including associative-list and set ownership, and is wired through
//! `server::rest`'s normal optimistic-concurrency persistence path.
//!
//! `GET`/`LIST`/`CREATE`/`UPDATE`/`PATCH` (all three real kinds now,
//! `JSON Patch`/`Merge Patch`/`strategic-merge-patch`)/`DELETE`/
//! `DELETECOLLECTION`/`WATCH`/the `status` subresource and
//! Server-Side Apply are all real for
//! CRD-defined resources now (`server::rest::resolve_resource`'s own doc
//! comment for the read/write verbs; `server::rest::resolve_dynamic_kind`
//! + `cacher::registry::CacheRegistry::spawn`, called lazily from
//! `server::listener`'s own `WATCH` dispatch on a resource's first-ever
//! watch request, for `WATCH`), and `CREATE`/`UPDATE`/`PATCH` now run
//! real pruning, then required/type validation, then defaulting against
//! a CRD's own schema — the same order real upstream's own CRD handler
//! runs them in (a field the schema doesn't declare is silently dropped
//! before validation ever sees it, matching that real behavior rather
//! than surfacing it as a spurious rejection). `discoverable_resources`
//! (this module's own registry submodule) also drives
//! `server::discovery`'s dynamic merge: a served, `Established` CRD's
//! resources now genuinely appear in `/apis`/`/apis/{group}`/
//! `/apis/{group}/{version}` and their aggregated-v2 counterparts, not
//! just at their own already-known URL.
//!
//! **Not yet landed, named honestly**: conversion webhooks; the
//! `status` subresource is now
//! schema-pruned and validated on `update_status`/`patch_status`, using the
//! matched version's `properties.status` schema. **Done, not a gap**: the
//! `status` subresource is genuinely gated on a CRD's own
//! `spec.versions[].subresources.status` (`registry::CrdResource::
//! has_status_subresource`) — a version that never declares it gets a
//! real `UnknownResource` from `update_status`/`patch_status`, not a
//! silent write.
//!
//! Status: in progress (Group K — see docs/APISERVER.md).

pub mod cel_evaluate;
pub mod cel_validations;
pub mod conditions;
pub mod conversion;
pub mod registry;
pub mod schema_defaults;
pub mod schema_apply;
pub mod schema_pruning;
pub mod schema_strategic_merge;
pub mod schema_validation;
