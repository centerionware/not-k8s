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
//! `GET`/`LIST`/`CREATE`/`DELETE`/`DELETECOLLECTION` are real for
//! CRD-defined resources now (`server::rest::resolve_resource`'s own doc
//! comment). **Not yet landed, named honestly**: `UPDATE`/`PATCH`/the
//! `status` subresource for CR objects (`server::rest`'s per-verb
//! functions still resolve purely through the static table for those,
//! so a CR update/patch is a real `404` today, not a silent no-op);
//! `WATCH` for CR objects (needs a dynamically-registered
//! `cacher::store::SharedCache` per Established CRD, spawned at runtime
//! rather than `server::listener`'s fixed boot-time
//! `BOOT_CACHED_RESOURCES` list — a genuinely separate feature); full
//! structural-schema type/required validation and pruning
//! (`x-kubernetes-preserve-unknown-fields`); `x-kubernetes-validations`
//! CEL; conversion webhooks; discovery merge (a CRD's resource doesn't
//! appear in `/apis/<group>/<version>` discovery output yet, even though
//! it's genuinely routable — Group E's discovery table is still the
//! static, build-time one).
//!
//! Status: in progress (Group K — see docs/APISERVER.md).

pub mod conditions;
pub mod registry;
pub mod schema_defaults;
