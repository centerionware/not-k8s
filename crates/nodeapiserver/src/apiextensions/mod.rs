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
//! identically.
//!
//! `GET`/`LIST`/`CREATE`/`UPDATE`/`PATCH` (`JSON Patch`/`Merge Patch`
//! only)/`DELETE`/`DELETECOLLECTION`/`WATCH`/the `status` subresource are
//! all real for CRD-defined resources now
//! (`server::rest::resolve_resource`'s own doc comment for the read/
//! write verbs; `server::rest::resolve_dynamic_kind` +
//! `cacher::registry::CacheRegistry::spawn`, called lazily from
//! `server::listener`'s own `WATCH` dispatch on a resource's first-ever
//! watch request, for `WATCH`), and `CREATE`/`UPDATE`/`PATCH` now run
//! real required/type validation against a CRD's own schema too
//! (`schema_validation`, wired the same place `schema_defaults` already
//! was). `strategic-merge-patch` against a CRD is a real, deliberate
//! exception — a clean `Invalid`, not a silently-wrong merge — since it
//! needs `x-kubernetes-list-type`/`-list-map-keys` resolution from a
//! *runtime* schema this crate has no interpreter for yet
//! (`crate::patch::strategic_merge` only walks a *compiled*
//! `ref_schema`). `discoverable_resources` (this module's own registry
//! submodule) also drives `server::discovery`'s dynamic merge: a served,
//! `Established` CRD's resources now genuinely appear in `/apis`/
//! `/apis/{group}`/`/apis/{group}/{version}` and their aggregated-v2
//! counterparts, not just at their own already-known URL.
//!
//! **Not yet landed, named honestly**: `x-kubernetes-preserve-unknown-
//! fields` pruning (a separate structural-schema concern — "should this
//! field even exist" rather than "is this field's value the right
//! shape" — `schema_validation` doesn't flag or strip an undeclared
//! field, it just skips it, same posture `scheme::validation` already
//! takes); enum membership, numeric ranges, format checks (RFC 1123
//! labels, ...) and any cross-field consistency rule
//! (`x-kubernetes-validations` CEL is a CRD schema's real mechanism for
//! all of that — needs the CEL cost budget built first, a named DoS
//! surface, not optional hardening); conversion webhooks; reacting to a
//! CRD's own lifecycle (a lazily-spawned watch reflector keeps running
//! even after its CRD is deleted, and a newly `Established` CRD is only
//! discovered by the next watch/discovery request for its resource, not
//! eagerly); gating the `status` subresource on a CRD's own
//! `spec.versions[].subresources.status` (currently always available,
//! unconditionally, for every CRD).
//!
//! Status: in progress (Group K — see docs/APISERVER.md).

pub mod conditions;
pub mod registry;
pub mod schema_defaults;
pub mod schema_validation;
