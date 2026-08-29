//! Group E: real, generic REST verbs wired against actual nodestore data.
//! Closes the gap `docs/APISERVER.md`
//! has named repeatedly ("actually wiring discovery/defaulting/validation
//! into a live request path"): everything this function needs (path
//! grammar, storage key layout, the protobuf wire codec, the discovery
//! table telling it which Kind a resource serves) already existed —
//! generic over every resource this build knows about, not hand-written
//! per type, same "generic over vendored data" posture every other Group
//! B/C/E slice has taken.
//!
//! # Scope, named honestly
//!
//! `GET` (single object, `GET /api/v1/namespaces/{ns}/pods/{name}`-shaped),
//! `LIST` (`GET /api/v1/namespaces/{ns}/pods`-shaped, no name), `CREATE`
//! (`POST /api/v1/namespaces/{ns}/pods`), single-object `DELETE`
//! (`DELETE /api/v1/namespaces/{ns}/pods/{name}`), `UPDATE`
//! (`PUT /api/v1/namespaces/{ns}/pods/{name}`), and now `PATCH`
//! (`PATCH /api/v1/namespaces/{ns}/pods/{name}`, real optimistic
//! concurrency against the exact revision `patch_prepare` itself reads,
//! no client-submitted `resourceVersion` required — see [`patch_prepare`]/
//! [`patch_persist`]'s own doc comments for the three real patch kinds,
//! reusing Group G's already-landed `patch::json_patch`/`merge_patch`/
//! `strategic_merge`, and for why the function is split in two —
//! `server::listener` runs Group J admission against the real candidate
//! object in between), and now [`delete_collection`]
//! (`DELETE /api/v1/namespaces/{ns}/pods`, no name — lists via the same
//! selector filtering [`list`] already has, then deletes each match,
//! returning the pre-deletion `List` real upstream's own
//! `Store.DeleteCollection` returns) — `watch` remains the only verb this
//! build knows about that isn't a generic REST dispatch (a real
//! streaming response instead, `server::listener`'s own doc comment).
//! **`DELETECOLLECTION` alone still doesn't run through Group J
//! admission**, a small named gap — see `server::listener`'s own doc
//! comment for why it's small in practice. `get` and `list` can both
//! consult a `cacher::store::SharedCache` if the caller passes one — see
//! each function's own doc comment for its exact contract (`get`: a hit
//! skips nodestore, a miss always falls through to a real `Range` rather
//! than trusting the cache to say "not found"; `list`: only once the
//! cache's own `has_synced()` is true, since an empty `list()` is a
//! valid answer on its own, not a fallthrough signal the way a `get`
//! miss is). `server::listener` actually does this for every built-in
//! resource in the generated discovery table; dynamically defined CRD
//! resources are still registered lazily after discovery. `create`/`update`/
//! `delete` still read/write
//! straight to `storage::client::StorageClient` directly, bypassing the
//! cache entirely — a real, valid strategy (upstream's own quorum-read /
//! watch-cache-disabled path takes exactly this shape), not a shortcut.
//! No authentication is consulted *inside*
//! this module either way — `server::listener` is what applies Group
//! H/I's identity/RBAC (opt-in, see that module's own doc comment)
//! before ever calling in here; Group J admission (five unconditional
//! plugins as of this revision — see `admission`'s own doc comment) is
//! applied in `server::listener`, also before dispatching in here.
//! The generic `<resource>/status` subresource is real now
//! (`update_status`/`patch_status`); every other subresource
//! (`pods/log`, ...) still isn't — the discovery table this module reads
//! doesn't carry them either (a named, separate skip in
//! `build/discovery_parse.rs`). `list` filters by label/field selector
//! for real (`cacher::selector::object_matches`, wired against every
//! item's own decoded JSON — Group D's own generic adapter, unchanged
//! here) and paginates for real too (`limit`/`continue_token`, its own
//! opaque resume-key encoding — see `list`'s own doc comment). `get` and
//! `list` also honor a positive `resourceVersion` by reading a consistent
//! nodestore MVCC snapshot; pinned requests bypass the live watch cache.
//!
//! `create` runs Group F's already-landed `scheme::validation`
//! (`validate_required`/`validate_types`, on the client's raw submitted
//! body — required-ness is about what the *user* sent, not what survives
//! defaulting, same order those functions' own doc comments already
//! specify) then `scheme::defaulting::apply_defaults`, sets
//! `metadata.creationTimestamp`/`uid` for real, and writes with a real
//! create-only-if-absent `Txn` (`Compare(ModRevision(key), Equal, 0)` —
//! confirmed directly against `nodestore`'s own server-side comment
//! naming this the idiom for "create only if absent," not assumed).
//! `name_format_violations` also wires `scheme::name_format`'s
//! validators in for the core-group resources this crate has actually
//! verified a real per-type rule for: `namespaces` -> `is_dns1123_label`
//! (`ValidateNamespaceName`), `services` -> `is_dns1035_label`
//! (`ValidateServiceName`, ignoring the alpha
//! `RelaxedServiceNameValidation` feature gate this crate has no
//! machinery for), and twenty-six resources sharing
//! `is_dns1123_subdomain` (core group: `serviceaccounts`, `pods`,
//! `replicationcontrollers`, `nodes`, `limitranges`, `resourcequotas`,
//! `secrets`, `endpoints`, `persistentvolumes`, `configmaps`; non-core,
//! each individually group-verified against the vendored spec:
//! `scheduling.k8s.io/priorityclasses`,
//! `resource.k8s.io/resourceclaims`,
//! `resource.k8s.io/resourceclaimtemplates`,
//! `storage.k8s.io/storageclasses`, `apps/controllerrevisions`,
//! `apps/daemonsets`, `apps/deployments`, `apps/replicasets`,
//! `networking.k8s.io/ingresses`, `networking.k8s.io/ingressclasses`,
//! `networking.k8s.io/servicecidrs`, `discovery.k8s.io/endpointslices`,
//! `flowcontrol.apiserver.k8s.io/flowschemas`,
//! `flowcontrol.apiserver.k8s.io/prioritylevelconfigurations`,
//! `node.k8s.io/runtimeclasses`, `coordination.k8s.io/leases`) — every
//! other resource is
//! deliberately left unchecked rather than guessed at; see that
//! function's own doc comment for how to extend it one verified entry at
//! a time. `update` runs the exact same two checks. `create` and `update`
//! also expose the listener's `dryRun=All` path, which returns the fully
//! prepared object without persisting it. Server-Side Apply bookkeeping is
//! handled by the separate apply path.
//!
//! `delete` reads the object, checks optional `resourceVersion`/`uid`
//! preconditions (`metav1.DeleteOptions.Preconditions`), and uses an MVCC
//! compare with `DeleteRange` so a concurrent update cannot invalidate the
//! check. It returns the deleted object, matching real upstream's own
//! synchronous delete response. `propagationPolicy` and finalizer handling
//! remain out of scope.
//!
//! `update` is real optimistic concurrency, not a blind overwrite: reads
//! the current object first, requires the submitted body's own
//! `metadata.resourceVersion` to match what's actually stored (a real
//! `Conflict`, not a silent clobber, on a mismatch — and a real
//! `MissingResourceVersion` outcome if the client omitted it, matching
//! real upstream's own requirement for `PUT`), then writes with a `Txn`
//! compared against that exact revision, so a concurrent write between
//! the read and this write also loses the race rather than being
//! silently overwritten. `metadata.creationTimestamp`/`uid` are always
//! preserved from the existing object regardless of what the client
//! submitted — both are immutable after creation, matching real
//! upstream. No create-on-update (a request targeting a name that
//! doesn't exist is rejected, not created — real upstream's
//! `AllowCreateOnUpdate` opt-in a handful of types use isn't modeled).

use crate::apiextensions;
use crate::cacher::selector::{self, ParseError};
use crate::codec::protobuf;
use crate::codegen;
use crate::scheme::{defaulting, validation};
use crate::storage::encryption::Transformer;
use crate::storage::client::{prefix_range_end, Error as StorageError, StorageClient};
use crate::storage::keys;
use crate::storage::pb::etcdserverpb as pb;
use crate::storage::pb::etcdserverpb::RangeRequest;
use crate::storage::pb::mvccpb;
use serde_json::{json, Value};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("nodestore request failed: {0}")]
    Storage(#[from] StorageError),
    #[error("decoding the stored object failed: {0}")]
    Decode(#[from] protobuf::Error),
    #[error("invalid selector: {0}")]
    Selector(#[from] ParseError),
    #[error("encryption transform failed: {0}")]
    Encryption(#[from] crate::storage::encryption::Error),
    #[error("invalid protobuf request: {0}")]
    InvalidProtobufRequest(String),
    #[error("the requested resource is not served")]
    UnknownResource,
}

#[derive(Debug, PartialEq)]
pub enum GetOutcome {
    /// The decoded object, ready to serialize.
    Found(Value),
    /// This build has no such `(group, version, resource)` at all — same
    /// "real 404, not a silent fallthrough" reasoning
    /// `server::discovery`'s own `NotFound` case already established.
    UnknownResource,
    /// The resource is known, but no object exists at that key.
    ObjectNotFound,
}

/// The `Kind` this build serves at `(group, version, resource)`, or
/// `None` if this build doesn't know that resource at all. Pure and
/// unit-tested apart from [`get`]'s own network call.
pub fn resolve_kind(group: &str, version: &str, resource: &str) -> Option<&'static str> {
    codegen::api_resources_by_group_version().get(&(group, version))?.iter().find(|r| r.resource == resource).map(|r| r.kind)
}

/// Resolves a parameter kind from a `ValidatingAdmissionPolicy`'s
/// `spec.paramKind`. Parameter kinds carry an API group and Kind but no
/// version or resource plural, so choose the most-preferred served version
/// from the static discovery table, then fall back to an Established CRD.
/// This is intentionally a read-only inverse of the normal resource lookup;
/// callers still use [`get`]` and [`list`]` for the actual parameter object.
pub async fn resolve_resource_for_kind(storage: &mut StorageClient, group: &str, kind: &str) -> Result<Option<(String, String, String, bool)>, Error> {
    let mut static_matches = codegen::api_resources::API_RESOURCES
        .iter()
        .filter(|resource| resource.group == group && resource.kind == kind)
        .collect::<Vec<_>>();
    static_matches.sort_by(|left, right| super::version_compare::compare_kube_aware_versions(&right.version, &left.version));
    if let Some(resource) = static_matches.into_iter().next() {
        return Ok(Some((resource.group.to_string(), resource.version.to_string(), resource.resource.to_string(), resource.namespaced)));
    }

    let mut dynamic_matches = apiextensions::registry::discoverable_resources(list_stored_crds(storage).await?.iter())
        .into_iter()
        .filter(|resource| resource.group == group && resource.kind == kind)
        .collect::<Vec<_>>();
    dynamic_matches.sort_by(|left, right| super::version_compare::compare_kube_aware_versions(&right.version, &left.version));
    Ok(dynamic_matches.into_iter().next().map(|resource| (resource.group, resource.version, resource.resource, resource.namespaced)))
}

/// What [`resolve_resource`] found `(group, version, resource)` to be —
/// either a built-in with a compiled proto schema (`resolve_kind`/
/// `schema_for_gvk`, unchanged from before Group K existed), or a
/// CRD-defined resource (`apiextensions::registry`), which has no
/// compiled schema at all: its body is stored/read as plain JSON, and
/// defaulting (when `open_api_schema` is present) walks that schema at
/// runtime instead of a compiled `FIELD_META` table
/// (`apiextensions::schema_defaults`).
struct ResolvedResource {
    kind: String,
    /// `Some(proto message name)` for a built-in; `None` for a CRD.
    schema: Option<&'static str>,
    open_api_schema: Option<Value>,
    /// Only ever meaningfully `true` for a CRD (`schema: None`) whose
    /// matched version declares `subresources.status` — always `true`
    /// for a static built-in, since this crate doesn't model per-type
    /// subresource declarations for built-ins at all yet (a real,
    /// separate, wider gap this field doesn't attempt to close — see
    /// `update_status`/`patch_status`'s own doc comment).
    has_status_subresource: bool,
}

/// The single place every real verb in this module decides what
/// `(group, version, resource)` actually is: the static, build-time
/// table first (no I/O, the overwhelmingly common case), falling back to
/// a live `LIST` of `CustomResourceDefinition`s only on a miss — Group
/// K's dynamic resource registry. `None` either way means a genuine
/// `UnknownResource` outcome to the caller, exactly as `resolve_kind`
/// alone used to mean.
///
/// **The CRD group itself is never recursed into** (`group.is_empty()`
/// covers the core group, which by definition has no CRDs in it
/// either): a request for `apiextensions.k8s.io/v1/customresourcedefinitions`
/// is always answered by the static table (Group A's codegen already
/// covers it — a `CustomResourceDefinition` is a real, compiled built-in
/// type, only the resources *it defines* are dynamic), so there's no risk
/// of this function ever listing CRDs to resolve a request for CRDs.
async fn resolve_resource(storage: &mut StorageClient, group: &str, version: &str, resource: &str) -> Result<Option<ResolvedResource>, Error> {
    if let Some(kind) = resolve_kind(group, version, resource) {
        return Ok(protobuf::schema_for_gvk(group, version, kind).map(|schema| ResolvedResource { kind: kind.to_string(), schema: Some(schema), open_api_schema: None, has_status_subresource: true }));
    }
    Ok(resolve_crd(storage, group, version, resource)
        .await?
        .map(|r| ResolvedResource { kind: r.kind, schema: None, open_api_schema: r.open_api_schema, has_status_subresource: r.has_status_subresource }))
}

/// The dynamic (CRD-only) half of [`resolve_resource`] — skips the
/// static `resolve_kind` check entirely, so it's only ever correct to
/// call once a caller has already ruled that out itself.
/// `server::listener`'s own `WATCH` dispatch is the other real caller
/// besides [`resolve_resource`]: `watch` is served straight from an
/// already-registered `cacher::store::SharedCache` rather than through
/// any of this module's own generic verb functions, so it has no other
/// reason to reach into `server::rest` for a CRD-defined resource at
/// all — it needs only the Kind a matching `Established` CRD resolves
/// to, both to spawn a cache for it on first watch
/// (`cacher::registry::CacheRegistry::spawn`, callable at any time, not
/// just at boot) and to label the watch events it then streams.
async fn resolve_crd(storage: &mut StorageClient, group: &str, version: &str, resource: &str) -> Result<Option<apiextensions::registry::CrdResource>, Error> {
    if group.is_empty() || group == "apiextensions.k8s.io" {
        return Ok(None);
    }
    let crds = list_stored_crds(storage).await?;
    Ok(apiextensions::registry::resolve_in(crds.iter(), group, version, resource))
}

/// Public wrapper around [`resolve_crd`] for `server::listener`'s own
/// `WATCH` dispatch (the one caller outside this module that needs
/// Group K's dynamic registry directly — every other verb goes through
/// [`resolve_resource`] instead, which this module keeps private).
pub async fn resolve_dynamic_kind(storage: &mut StorageClient, group: &str, version: &str, resource: &str) -> Result<Option<String>, Error> {
    Ok(resolve_crd(storage, group, version, resource).await?.map(|r| r.kind))
}

/// Every stored `CustomResourceDefinition`, decoded — `server::listener`'s
/// own discovery-merge call site is the other real caller outside this
/// module that needs the raw documents (not just one resolved GVR): it
/// merges every served, `Established` CRD's own resources into
/// `/apis`/`/apis/{group}`/`/apis/{group}/{version}` discovery output
/// (`apiextensions::registry::discoverable_resources` does the actual
/// filtering/shaping). Public so that call site doesn't need its own
/// copy of the raw-`Range`-plus-decode this module already has.
pub async fn list_all_crds(storage: &mut StorageClient) -> Result<Vec<Value>, Error> {
    list_stored_crds(storage).await
}

/// A raw `Range` over every stored `CustomResourceDefinition`, decoded —
/// deliberately *not* [`list`] itself: [`list`] calls [`resolve_resource`]
/// to find out what it's listing, and [`resolve_resource`]'s own CRD
/// fallback needs this same data, so calling back into `list` here would
/// be a real `async fn` recursion cycle (rejected outright by rustc,
/// `E0733` — infinitely-sized future, not merely a style objection) even
/// though it would never actually recurse more than once at runtime (the
/// CRD group is always resolved by the static table, never this
/// fallback). `customresourcedefinitions` is always cluster-scoped and
/// its own resource is never itself encrypted-at-rest-configurable in a
/// way this function needs to special-case — `decrypt_and_decode`
/// already handles "no transformer configured for this group/resource"
/// as a plain pass-through.
async fn list_stored_crds(storage: &mut StorageClient) -> Result<Vec<Value>, Error> {
    let prefix = keys::list_prefix("apiextensions.k8s.io", "customresourcedefinitions", None).into_bytes();
    let range_end = prefix_range_end(&prefix);
    let resp = storage.range(RangeRequest { key: prefix, range_end, ..Default::default() }).await?;
    resp.kvs.iter().map(|kv| decrypt_and_decode(storage, "apiextensions.k8s.io", "customresourcedefinitions", &kv.key, &kv.value)).collect()
}

/// Decodes a value exactly as stored in nodestore — the full `k8s\0`-
/// prefixed `runtime.Unknown` envelope `codec::protobuf::wrap_unknown`
/// produces — back into JSON. Pure and unit-tested with a real encoded
/// round trip, no network involved. Resolves the schema from the
/// envelope's own `apiVersion`/`kind` (what was actually written), not
/// from the caller's request path, so a decode is always faithful to
/// what's really stored even if the two ever disagreed.
pub fn decode_stored_object(bytes: &[u8]) -> Result<Value, protobuf::Error> {
    let (api_version, kind, object_bytes) = protobuf::unwrap_unknown(bytes)?;
    let (group, version) = split_api_version(&api_version);
    let mut object = match protobuf::schema_for_gvk(group, version, &kind) {
        Some(schema) => protobuf::decode_message(schema, &object_bytes),
        // Group K: no compiled schema for this Kind at all -- a CRD-
        // defined object, which `server::rest`'s write side always
        // stores as raw JSON in the envelope's `raw` field rather than
        // protobuf-encoding it (there's no compiled schema to encode
        // *with* either). A genuinely unknown, non-CRD Kind decodes to
        // the same `Json` error a malformed CRD body would -- this
        // function has no registry to tell the two apart, and both are
        // real "can't decode this" outcomes either way.
        None => Ok(serde_json::from_slice(&object_bytes).map_err(protobuf::Error::Json)?),
    }?;
    set_type_metadata(&mut object, &kind, &api_version);
    Ok(object)
}

/// Decodes a Kubernetes protobuf request envelope after resolving the
/// resource named by the URL. Built-in resources use their generated schema;
/// CRD objects use the envelope's raw JSON body because Kubernetes does not
/// generate a compiled protobuf schema for operator-defined kinds.
pub async fn decode_protobuf_request(
    storage: &mut StorageClient,
    group: &str,
    version: &str,
    resource: &str,
    bytes: &[u8],
) -> Result<Option<Value>, Error> {
    let Some(resolved) = resolve_resource(storage, group, version, resource).await? else {
        return Ok(None);
    };
    Ok(Some(decode_protobuf_object(&resolved, resource, bytes)?))
}

fn decode_protobuf_object(resolved: &ResolvedResource, resource: &str, bytes: &[u8]) -> Result<Value, Error> {
    let (api_version, kind, object_bytes) = protobuf::unwrap_unknown(bytes)?;
    if kind != resolved.kind {
        return Err(Error::InvalidProtobufRequest(format!("request kind {kind:?} does not match resource {resource:?}")));
    }
    let (body_group, body_version) = split_api_version(&api_version);
    let mut object = match resolved.schema.or_else(|| protobuf::schema_for_gvk(body_group, body_version, &kind)) {
        Some(schema) => protobuf::decode_message(schema, &object_bytes)?,
        None => serde_json::from_slice(&object_bytes).map_err(protobuf::Error::Json)?,
    };
    set_type_metadata(&mut object, &kind, &api_version);
    Ok(object)
}

/// Group C: the encrypted-aware counterpart to [`decode_stored_object`] —
/// decrypts `bytes` first when `storage` has a matching transformer for
/// `(group, resource)`, else decodes them as-is. Every real read call
/// site in this module (`get`, `list`, `update`'s own existing-object
/// read, `patch_prepare`, `update_status`, `patch_status`, `delete`, ...)
/// uses this instead of calling `decode_stored_object` directly, so
/// decryption happens in exactly one place regardless of which read path
/// is asking — the same centralization
/// `storage::encryption_config`'s own module doc comment named as the
/// reason this wiring was deferred until it could be done once, for
/// everything, rather than gap-by-gap.
///
/// `key` is the object's own real etcd key — required as AES-GCM's
/// authenticated data (`storage::encryption::Transformer`'s own doc
/// comment: "so a ciphertext can't be copied to a different key and
/// still decrypt"), matching real upstream's own
/// `dataCtx.AuthenticatedData()` convention exactly. The real upstream
/// `stale` flag `transform_from_storage` returns (real upstream's own
/// signal that a value was encrypted under a non-primary key — a
/// migration-in-progress marker meaning "rewrite this with the current
/// primary key next time it's written") is intentionally discarded here:
/// this build has nowhere to act on it yet (no background re-encryption
/// sweep), a named, narrower gap than the wiring itself, not silently
/// dropped without comment.
pub(crate) fn decrypt_and_decode(storage: &StorageClient, group: &str, resource: &str, key: &[u8], bytes: &[u8]) -> Result<Value, Error> {
    match storage.transformers_for(group, resource) {
        Some(transformers) => {
            let (plaintext, _stale) = transformers.transform_from_storage(bytes, key)?;
            Ok(decode_stored_object(&plaintext)?)
        }
        None => Ok(decode_stored_object(bytes)?),
    }
}

/// The write-side counterpart to [`decrypt_and_decode`]: encrypts `bytes`
/// (a real `wrap_unknown` envelope) when `storage` has a matching
/// transformer for `(group, resource)`, else returns it unchanged. Both
/// real `PutRequest` construction sites in this crate (`create`,
/// `persist_update`, the latter shared by `update`/`patch`/
/// `update_status`/`patch_status`) call this immediately before building
/// the request — nothing this crate writes to nodestore ever bypasses
/// this when encryption is actually configured for its resource.
pub(crate) fn encrypt_for_storage(storage: &StorageClient, group: &str, resource: &str, key: &[u8], bytes: &[u8]) -> Result<Vec<u8>, Error> {
    match storage.transformers_for(group, resource) {
        Some(transformers) => Ok(transformers.transform_to_storage(bytes, key)?),
        None => Ok(bytes.to_vec()),
    }
}

/// `""` -> `("", "")` (never real — `apiVersion` is empty only for a
/// malformed/never-written envelope), `"v1"` -> `("", "v1")` (the core
/// group has no group segment in `apiVersion`), `"apps/v1"` ->
/// `("apps", "v1")`.
fn split_api_version(api_version: &str) -> (&str, &str) {
    match api_version.split_once('/') {
        Some((group, version)) => (group, version),
        None => ("", api_version),
    }
}

/// Fetches and decodes a single object. `namespace` is `None` for a
/// cluster-scoped resource (matches `storage::keys::object_key`'s own
/// convention) — the caller (`server::listener`) is responsible for
/// turning `path::RequestInfo`'s always-`String` `namespace` field into
/// this `Option` (empty string -> `None`).
///
/// `cache`, if given, is consulted first (`cacher::store::SharedCache::get`,
/// Group D) — a hit skips the `Range` round trip to nodestore entirely.
/// A **miss always falls through to nodestore**, unconditionally, rather
/// than trusting the cache to say "not found": a `cache: Some(_)` that
/// hasn't finished its first `LIST` yet (or isn't registered for this
/// exact resource at all, if a caller ever passed the wrong one) is
/// indistinguishable from "genuinely empty" using only what `SharedCache`
/// exposes today, so treating a miss as authoritative would risk a false
/// `404` during that window. This makes cache consultation a pure
/// latency optimization on the hit path, never a correctness risk on the
/// miss path — real upstream's own watch cache takes the same
/// "consistent read falls through" posture for exactly this reason.
/// `None` behaves exactly as before this parameter existed; callers outside
/// the listener's cache path can still pass `None`.
pub async fn get(storage: &mut StorageClient, cache: Option<&crate::cacher::store::SharedCache>, group: &str, version: &str, resource: &str, namespace: Option<&str>, name: &str) -> Result<GetOutcome, Error> {
    get_at_revision(storage, cache, group, version, resource, namespace, name, 0).await
}

/// [`get`] with an optional etcd MVCC snapshot revision. A non-positive
/// revision retains the normal current-state behavior.
pub async fn get_at_revision(storage: &mut StorageClient, cache: Option<&crate::cacher::store::SharedCache>, group: &str, version: &str, resource: &str, namespace: Option<&str>, name: &str, resource_version: i64) -> Result<GetOutcome, Error> {
    let Some(resolved) = resolve_resource(storage, group, version, resource).await? else {
        return Ok(GetOutcome::UnknownResource);
    };
    let kind = resolved.kind;
    let key = keys::object_key(group, resource, namespace, name);

    if resource_version <= 0 {
        if let Some(cache) = cache {
            if let Some(entry) = cache.get(key.as_bytes()) {
                let mut object = decrypt_and_decode(storage, group, resource, key.as_bytes(), &entry.value)?;
                set_metadata_field(&mut object, "resourceVersion", Value::String(entry.mod_revision.to_string()));
                return Ok(GetOutcome::Found(crate::scheme::conversion::to_version(group, version, &kind, object)));
            }
        }
    }

    let resp = storage.range(RangeRequest { key: key.into_bytes(), revision: resource_version.max(0), ..Default::default() }).await?;
    let Some(kv) = resp.kvs.into_iter().next() else {
        return Ok(GetOutcome::ObjectNotFound);
    };
    let mut object = decrypt_and_decode(storage, group, resource, &kv.key, &kv.value)?;
    // Real, load-bearing fix, found live (`tests/apiservice_roundtrip.rs`'s
    // own get-then-update round trip): `resourceVersion` is never
    // actually *persisted* into the stored object bytes (`create`/
    // `persist_update` both stamp it onto their own return value only
    // *after* the write that produced it — the revision doesn't exist
    // yet while the bytes being written are still being built, so there
    // is nothing earlier to persist it into either) — matching real
    // upstream's own posture, where `resourceVersion` is always etcd's
    // own `mod_revision` read back at serve time, never object content.
    // A plain read has to do the same real-time stamping every write
    // path already does, from this exact `Range`'s own `kv.mod_revision`
    // — every prior write-then-read-back test in this crate happened to
    // use a `create`/`update` call's own return value directly, which
    // already carried a real `resourceVersion`, so nothing exercised a
    // genuine `GET` followed by an `UPDATE` until this one did.
    set_metadata_field(&mut object, "resourceVersion", Value::String(kv.mod_revision.to_string()));
    Ok(GetOutcome::Found(crate::scheme::conversion::to_version(group, version, &kind, object)))
}

#[derive(Debug, PartialEq)]
pub enum ListOutcome {
    /// The real `<Kind>List` document, ready to serialize.
    Found(Value),
    UnknownResource,
    /// The submitted `continue` token didn't decode — not valid base64,
    /// no `0x00` key/revision separator, or a non-numeric revision.
    /// Real upstream's own `errors.NewBadRequest("continue token is not
    /// valid")` shape, not a `500`.
    InvalidContinueToken,
}

/// The real `<Kind>List` `kind` value for a resource this build serves —
/// standard Kubernetes convention, verified against real vendored data:
/// every List type in the vendored OpenAPI specs is named exactly
/// `<Kind>List` (`PodList`, `DeploymentList`, ...), never a separate
/// hand-assigned name.
fn list_kind(kind: &str) -> String {
    format!("{kind}List")
}

/// Lists every object of a resource — the whole resource, or scoped to
/// one namespace (`namespace: None` for a cluster-scoped resource, same
/// convention as [`get`]). Items are decoded and filtered
/// (`cacher::selector::object_matches`) the same way regardless of source.
/// Items are returned in whatever order the source hands them back in
/// (key order, for both a real `Range` and the cache's own `BTreeMap`) —
/// real upstream doesn't guarantee list ordering either.
/// `label_selector`/`field_selector` are the raw query-string values
/// `path::RequestInfo` already captures for `list` (empty means "no
/// constraint from that half," matching upstream's own `Everything()`
/// selector semantics). `limit`/`continue_token` are real pagination —
/// `limit <= 0` means "no limit" (matching real upstream's own `0`
/// convention), and a non-empty `continue_token` resumes an earlier
/// paginated listing (real upstream's own contract: opaque to the
/// client, only ever handed back verbatim from a prior page's own
/// `metadata.continue`). A paginated request always bypasses the watch
/// cache (see below) and reads directly from nodestore, since real
/// pagination is a genuine ordered range-scan-with-resume-point, which
/// the cache's own unordered in-memory store doesn't support. Real
/// upstream's own documented caveat applies here too: label/field
/// selector filtering happens *after* the limited range fetch, so a
/// page can come back with fewer than `limit` items (even zero) despite
/// more matching items existing on later pages.
///
/// `cache`, if given, is consulted first — but only once
/// [`crate::cacher::store::SharedCache::has_synced`] is true. Unlike
/// [`get`]'s "a miss always falls through" trick, `list` can't use that
/// same safety net: a cache that hasn't finished its first `LIST` yet
/// would report zero items, and zero items is itself a fully valid `LIST`
/// answer (a real `200`, not a `404`) — there is no way to tell "empty
/// because unsynced" from "empty because genuinely empty" after the fact,
/// so this checks `has_synced()` up front instead (see that method's own
/// doc comment for why it's a real flag, not inferred from the revision).
/// An unsynced cache falls through to nodestore exactly as `cache: None`
/// would. `None` behaves exactly as before this parameter existed; callers
/// outside the listener's cache path still pass `None` (same scope as
/// `get`'s own cache parameter).
pub async fn list(
    storage: &mut StorageClient,
    cache: Option<&crate::cacher::store::SharedCache>,
    group: &str,
    version: &str,
    resource: &str,
    namespace: Option<&str>,
    label_selector: &str,
    field_selector: &str,
    limit: i64,
    continue_token: &str,
) -> Result<ListOutcome, Error> {
    list_at_revision(storage, cache, group, version, resource, namespace, label_selector, field_selector, limit, continue_token, 0).await
}

/// [`list`] with an optional etcd MVCC snapshot revision. A positive
/// revision bypasses the live watch cache and returns a consistent snapshot
/// from nodestore.
pub async fn list_at_revision(
    storage: &mut StorageClient,
    cache: Option<&crate::cacher::store::SharedCache>,
    group: &str,
    version: &str,
    resource: &str,
    namespace: Option<&str>,
    label_selector: &str,
    field_selector: &str,
    limit: i64,
    continue_token: &str,
    resource_version: i64,
) -> Result<ListOutcome, Error> {
    let Some(resolved) = resolve_resource(storage, group, version, resource).await? else {
        return Ok(ListOutcome::UnknownResource);
    };
    let kind = resolved.kind.as_str();
    let label_reqs = if label_selector.is_empty() { Vec::new() } else { selector::parse_label_selector(label_selector)? };
    let field_reqs = if field_selector.is_empty() { Vec::new() } else { selector::parse_field_selector(field_selector)? };
    selector::validate_field_selector(group, resource, &field_reqs)?;

    let group_version = if group.is_empty() { version.to_string() } else { format!("{group}/{version}") };
    // Shared by both the cache path and the direct-nodestore path below —
    // the cache registers one entry per whole `(group, version, resource)`
    // (`cacher::registry`'s own doc comment: "every namespace at once, not
    // one cache per namespace"), so a namespaced request still needs this
    // same prefix to scope the cache's own entries down to one namespace,
    // exactly as it already scopes the `Range` request on the fallback path.
    let prefix = keys::list_prefix(group, resource, namespace).into_bytes();

    // Real upstream itself doesn't serve a paginated request from its own
    // watch cache either — a consistent ordered range-scan-with-resume-point
    // is what the underlying store gives for free and an in-memory
    // unordered cache doesn't. A paginated request (real `limit`/`continue`,
    // not the default "everything") always goes straight to nodestore
    // below, same as an unsynced cache would.
    let paginated = limit > 0 || !continue_token.is_empty() || resource_version > 0;
    if let Some(cache) = cache {
        if cache.has_synced() && !paginated {
            let (entries, revision) = cache.list();
            let items = entries
                .iter()
                .filter(|(key, _)| key.starts_with(&prefix))
                .map(|(key, entry)| {
                    // Same real fix `get`'s own doc comment covers: a
                    // stored object never carries `resourceVersion` as
                    // persisted content, so every item in a `LIST`
                    // response needs it stamped from its own live
                    // revision, the same way real upstream does.
                    let mut object = decrypt_and_decode(storage, group, resource, key, &entry.value)?;
                    set_metadata_field(&mut object, "resourceVersion", Value::String(entry.mod_revision.to_string()));
                    object = crate::scheme::conversion::to_version(group, version, kind, object);
                    Ok(object)
                })
                .collect::<Result<Vec<Value>, Error>>()?
                .into_iter()
                .filter(|item| selector::object_matches(item, &label_reqs, &field_reqs))
                .collect::<Vec<Value>>();
            return Ok(ListOutcome::Found(json!({
                "kind": list_kind(kind),
                "apiVersion": group_version,
                "metadata": {"resourceVersion": revision.to_string()},
                "items": items,
            })));
        }
    }

    let range_end = prefix_range_end(&prefix);
    // A `continue` token resumes from the exact key its own page left off
    // at (`encode_continue_token`'s own doc comment covers the "append a
    // single 0x00 byte" idiom that makes this the correct etcd range
    // start), at the same revision the listing began at — every page of
    // one listing sees a consistent snapshot, matching real upstream's
    // own pagination contract.
    let (start_key, at_revision) = if continue_token.is_empty() {
        (prefix, resource_version.max(0))
    } else {
        match decode_continue_token(continue_token) {
            Some((key, revision)) => (key, revision),
            None => return Ok(ListOutcome::InvalidContinueToken),
        }
    };
    let resp = storage.range(RangeRequest { key: start_key, range_end, limit: limit.max(0), revision: at_revision, ..Default::default() }).await?;
    let revision = resp.header.map(|h| h.revision).unwrap_or(at_revision);
    // Real upstream's own documented caveat applies here too: filtering by
    // label/field selector happens *after* the limited range fetch, so a
    // page can legitimately come back with fewer than `limit` items (or
    // even zero) despite there being more matching items on later pages —
    // this isn't a bug, it's the same trade-off a selector combined with
    // `limit` has against a real etcd-backed apiserver.
    let more = resp.more;
    // The successor marker `encode_continue_token`'s own doc comment
    // expects — appended *here*, not inside that function, so its own
    // internal `0x00` push stays purely about the encoding's key/revision
    // separator (see that function's doc comment for why the two
    // 0x00 bytes this produces when they land back to back is
    // deliberate, not a bug).
    let resume_key = resp.kvs.last().map(|kv| {
        let mut k = kv.key.clone();
        k.push(0);
        k
    });
    let items = resp
        .kvs
        .iter()
        .map(|kv| {
            let mut object = decrypt_and_decode(storage, group, resource, &kv.key, &kv.value)?;
            set_metadata_field(&mut object, "resourceVersion", Value::String(kv.mod_revision.to_string()));
            object = crate::scheme::conversion::to_version(group, version, kind, object);
            Ok(object)
        })
        .collect::<Result<Vec<Value>, Error>>()?
        .into_iter()
        .filter(|item| selector::object_matches(item, &label_reqs, &field_reqs))
        .collect::<Vec<Value>>();

    let mut metadata = json!({"resourceVersion": revision.to_string()});
    if more {
        if let Some(resume_key) = resume_key {
            metadata["continue"] = json!(encode_continue_token(&resume_key, revision));
        }
    }

    Ok(ListOutcome::Found(json!({
        "kind": list_kind(kind),
        "apiVersion": group_version,
        "metadata": metadata,
        "items": items,
    })))
}

/// Real upstream's own continuation-token contract: a client must treat
/// this as fully opaque, never construct or parse one itself. This
/// build's own encoding (base64 of `<resume-key>\0<revision>`) has no
/// compatibility requirement with real upstream's own token format,
/// since nothing outside this crate's own client/server pair ever reads
/// one.
///
/// `resume_key` must already be `list`'s own last-returned key with a
/// single `0x00` byte appended by the caller (the standard etcd idiom
/// for "the immediate lexicographic successor of this key" — exactly
/// the correct next `Range` start to exclude everything already
/// returned while including everything after it: byte-string
/// comparison guarantees any real key strictly greater than `last_key`
/// is always >= `last_key + 0x00`, since `0x00` is the smallest
/// possible byte). This function then appends *its own* `0x00` as the
/// key/revision separator — so a real encoded buffer ends up with two
/// consecutive `0x00` bytes where the successor marker meets the
/// separator, which is deliberate, not a bug: [`decode_continue_token`]
/// finds the *last* one to split on, so the successor marker correctly
/// stays part of the decoded key.
fn encode_continue_token(resume_key: &[u8], revision: i64) -> String {
    use base64::Engine;
    let mut buf = resume_key.to_vec();
    buf.push(0);
    buf.extend_from_slice(revision.to_string().as_bytes());
    base64::engine::general_purpose::STANDARD.encode(buf)
}

/// The inverse of [`encode_continue_token`]. `None` for anything
/// malformed (not valid base64, no `0x00` separator, a non-numeric
/// revision) — surfaced by `list` as a real `ListOutcome::
/// InvalidContinueToken`, not a panic or a silently-wrong resume point.
/// Splits on the *last* `0x00` byte rather than the first, defensively:
/// a resume key built from real object names should never itself
/// contain one (`DNS-1123` names have no room for a null byte), but
/// searching from the end costs nothing and removes even that
/// assumption.
fn decode_continue_token(token: &str) -> Option<(Vec<u8>, i64)> {
    use base64::Engine;
    let buf = base64::engine::general_purpose::STANDARD.decode(token).ok()?;
    let separator = buf.iter().rposition(|&b| b == 0)?;
    let (key, rest) = buf.split_at(separator);
    let revision = std::str::from_utf8(&rest[1..]).ok()?.parse::<i64>().ok()?;
    Some((key.to_vec(), revision))
}

#[derive(Debug, PartialEq)]
pub enum CreateOutcome {
    /// The stored object, exactly as written (defaults applied,
    /// `creationTimestamp`/`uid`/`resourceVersion` set for real).
    Created(Value),
    UnknownResource,
    /// Neither `metadata.name` nor a usable `metadata.generateName` was
    /// present in the submitted body.
    MissingName,
    /// `metadata.namespace` in the body disagreed with the URL's own
    /// namespace — real upstream rejects this rather than silently
    /// preferring one over the other.
    NamespaceMismatch,
    /// An object already exists at this key — real upstream's own
    /// `AlreadyExists` outcome.
    AlreadyExists,
    /// `scheme::validation`'s own findings, formatted as one message per
    /// violation (`"containers[1].name: Required value"`-shaped) — the
    /// caller's job to turn into a real `422 Unprocessable Entity`.
    Invalid(Vec<String>),
}

/// Creates a new object. `namespace: None` for a cluster-scoped resource,
/// same convention as [`get`]/[`list`]. `body` is the client's raw
/// submitted object, decoded but otherwise untouched — this function
/// validates and defaults it, it doesn't trust it.
pub async fn create(storage: &mut StorageClient, group: &str, version: &str, resource: &str, namespace: Option<&str>, body: &Value) -> Result<CreateOutcome, Error> {
    create_with_options(storage, group, version, resource, namespace, body, false).await
}

/// [`create`] with the real Kubernetes `dryRun=All` write option. Dry-run
/// still resolves, validates, defaults, and checks for an existing object,
/// but never changes nodestore.
pub async fn create_with_options(storage: &mut StorageClient, group: &str, version: &str, resource: &str, namespace: Option<&str>, body: &Value, dry_run: bool) -> Result<CreateOutcome, Error> {
    let Some(resolved) = resolve_resource(storage, group, version, resource).await? else {
        return Ok(CreateOutcome::UnknownResource);
    };
    let kind = resolved.kind.as_str();

    let explicit_name = body.pointer("/metadata/name").and_then(Value::as_str).filter(|n| !n.is_empty());
    let generated_prefix = body.pointer("/metadata/generateName").and_then(Value::as_str).filter(|prefix| !prefix.is_empty());
    let Some(name) = explicit_name.map(str::to_string).or_else(|| generated_prefix.map(generate_name)) else {
        return Ok(CreateOutcome::MissingName);
    };
    let mut submitted_body = body.clone();
    if explicit_name.is_none() {
        set_metadata_field(&mut submitted_body, "name", Value::String(name.clone()));
    }
    let body = &submitted_body;

    if let (Some(ns), Some(body_ns)) = (namespace, body.pointer("/metadata/namespace").and_then(Value::as_str)) {
        if !body_ns.is_empty() && body_ns != ns {
            return Ok(CreateOutcome::NamespaceMismatch);
        }
    }

    // Group K: structural-schema pruning runs before validation/defaulting,
    // matching real upstream's own order — a field the schema doesn't
    // declare is silently dropped here rather than surfacing as a
    // validation error, the same way real upstream's own CRD handler
    // behaves (`apiextensions::schema_pruning`'s own doc comment).
    let pruned_body;
    let body: &Value = match &resolved.open_api_schema {
        Some(open_api_schema) => {
            pruned_body = apiextensions::schema_pruning::prune(open_api_schema, body);
            &pruned_body
        }
        None => body,
    };

    let mut violations: Vec<String> = match (resolved.schema, &resolved.open_api_schema) {
        (Some(schema), _) => {
            let mut v: Vec<String> = validation::validate_required(schema, body).into_iter().map(|m| format!("{}: Required value", m.path)).collect();
            v.extend(validation::validate_types(schema, body).into_iter().map(|t| format!("{}: expected type {}, got {}", t.path, t.expected, t.actual_kind)));
            v
        }
        // Group K: real required/type validation against a CRD's own
        // openAPIV3Schema, when it has one.
        (None, Some(open_api_schema)) => {
            let mut v: Vec<String> = apiextensions::schema_validation::validate_required(open_api_schema, body).into_iter().map(|m| format!("{}: Required value", m.path)).collect();
            v.extend(apiextensions::schema_validation::validate_types(open_api_schema, body).into_iter().map(|t| format!("{}: expected type {}, got {}", t.path, t.expected, t.actual_kind)));
            v.extend(apiextensions::schema_validation::validate_constraints(open_api_schema, body));
            v
        }
        (None, None) => Vec::new(),
    };
    violations.extend(name_format_violations(group, resource, &name).into_iter().map(|e| format!("metadata.name: {e}")));
    // Group K / CEL Phase 3: a CustomResourceDefinition's own
    // `x-kubernetes-validations` rules get their real static cost
    // checked at CRD-acceptance time, real upstream's own posture
    // (`apiextensions::cel_validations`'s own doc comment covers the
    // exact real scope and its one named gap — no `MaxCardinality`
    // multiplication yet).
    if group == "apiextensions.k8s.io" && resource == "customresourcedefinitions" {
        violations.extend(apiextensions::cel_validations::validate_crd_cel_costs(body));
    }
    if !violations.is_empty() {
        return Ok(CreateOutcome::Invalid(violations));
    }

    let mut object = match (resolved.schema, &resolved.open_api_schema) {
        (Some(schema), _) => defaulting::apply_defaults(schema, body),
        (None, Some(open_api_schema)) => apiextensions::schema_defaults::apply_defaults(open_api_schema, body),
        (None, None) => body.clone(),
    };
    object = crate::scheme::conversion::to_version(group, version, kind, object);

    // CEL Phase 4: real x-kubernetes-validations rule evaluation against
    // this actual custom resource instance — runs against the
    // fully-defaulted object (real upstream's own ordering: a rule
    // commonly assumes a field already carries its real default, not an
    // absence), `old_value: None` on `CREATE` (real upstream's own
    // `oldSelf` is simply unavailable then, matching
    // `apiextensions::cel_evaluate`'s own doc comment).
    if let Some(open_api_schema) = &resolved.open_api_schema {
        let rule_violations = apiextensions::cel_evaluate::validate_object(open_api_schema, &object, None);
        if !rule_violations.is_empty() {
            return Ok(CreateOutcome::Invalid(rule_violations.into_iter().map(|v| v.to_string()).collect()));
        }
    }

    set_metadata_field(&mut object, "creationTimestamp", Value::String(now_rfc3339()));
    set_metadata_field(&mut object, "uid", Value::String(uuid::Uuid::new_v4().to_string()));
    if let Some(ns) = namespace {
        set_metadata_field(&mut object, "namespace", Value::String(ns.to_string()));
    }

    // Group K: a CustomResourceDefinition's own `status` is entirely
    // server-computed (`apiextensions::conditions`'s own doc comment
    // covers why this build computes it synchronously right here rather
    // than through a separate async establishing controller) — never
    // trusted from whatever the client's submitted body carried under
    // `status`, same "generic status subresource" posture `update_status`
    // already establishes for every other resource's own status.
    if group == "apiextensions.k8s.io" && resource == "customresourcedefinitions" {
        let other_crds = list_stored_crds(storage).await?;
        object["status"] = apiextensions::conditions::compute_status(&object, other_crds.iter(), &[], &now_rfc3339());
    }

    let key = keys::object_key(group, resource, namespace, &name);
    if dry_run {
        let existing = storage.range(RangeRequest { key: key.clone().into_bytes(), ..Default::default() }).await?;
        if !existing.kvs.is_empty() {
            return Ok(CreateOutcome::AlreadyExists);
        }
        return Ok(CreateOutcome::Created(object));
    }
    let api_version = if group.is_empty() { version.to_string() } else { format!("{group}/{version}") };
    let object_bytes = match resolved.schema {
        Some(schema) => protobuf::encode_message(schema, &object)?,
        None => serde_json::to_vec(&object).map_err(protobuf::Error::Json)?,
    };
    let envelope = protobuf::wrap_unknown(&api_version, kind, &object_bytes);

    // Real upstream's own create-only-if-absent idiom, confirmed against
    // nodestore's own server-side comment naming it exactly this
    // (`crates/nodestore/src/store.rs`): a key with no prior write has
    // ModRevision 0, so a Txn that only Puts when ModRevision == 0 can
    // never silently overwrite an existing object.
    let compare = pb::Compare {
        key: key.clone().into_bytes(),
        result: pb::compare::CompareResult::Equal as i32,
        target: pb::compare::CompareTarget::Mod as i32,
        target_union: Some(pb::compare::TargetUnion::ModRevision(0)),
        range_end: Vec::new(),
    };
    let envelope = encrypt_for_storage(storage, group, resource, key.as_bytes(), &envelope)?;
    let put = pb::PutRequest { key: key.into_bytes(), value: envelope, ..Default::default() };
    let txn = pb::TxnRequest {
        compare: vec![compare],
        success: vec![pb::RequestOp { request: Some(pb::request_op::Request::RequestPut(put)) }],
        failure: vec![],
    };
    let resp = storage.txn(txn).await?;
    if !resp.succeeded {
        return Ok(CreateOutcome::AlreadyExists);
    }

    let revision = resp.header.map(|h| h.revision).unwrap_or(0);
    set_metadata_field(&mut object, "resourceVersion", Value::String(revision.to_string()));
    Ok(CreateOutcome::Created(object))
}

#[derive(Debug, PartialEq)]
pub enum UpdateOutcome {
    Updated(Value),
    UnknownResource,
    /// No object exists at this key — this build doesn't support
    /// create-on-update (`AllowCreateOnUpdate`, real upstream's own
    /// opt-in a handful of types use), named honestly rather than
    /// silently creating one.
    ObjectNotFound,
    /// The submitted body had no `metadata.resourceVersion` at all —
    /// real upstream's own generic registry requires one for `PUT`
    /// (optimistic concurrency has nothing to compare against
    /// otherwise).
    MissingResourceVersion,
    /// The submitted `resourceVersion` didn't match what's currently
    /// stored — a real conflict, matching real upstream's own
    /// `errors.NewConflict`.
    Conflict,
    NamespaceMismatch,
    Invalid(Vec<String>),
    /// [`patch`] only: the `Content-Type` wasn't one of the three real
    /// patch media types this build understands.
    UnsupportedPatchType,
}

/// Replaces an existing object. `namespace: None` for a cluster-scoped
/// resource, same convention as [`get`]/[`create`]. Real optimistic
/// concurrency: reads the current object first, requires the submitted
/// body's own `metadata.resourceVersion` to match what's actually
/// stored, and writes with a `Txn` compared against that same revision
/// — a concurrent write between the read and this write loses the race
/// and gets a real `Conflict`, not a silent overwrite.
/// `metadata.creationTimestamp`/`uid` are preserved from the existing
/// object regardless of what the client submitted — real upstream
/// treats both as immutable after creation.
pub async fn update(storage: &mut StorageClient, group: &str, version: &str, resource: &str, namespace: Option<&str>, name: &str, body: &Value) -> Result<UpdateOutcome, Error> {
    update_with_options(storage, group, version, resource, namespace, name, body, false).await
}

/// [`update`] with the real Kubernetes `dryRun=All` write option. The
/// candidate is prepared exactly like a normal update, but the final
/// optimistic-concurrency write is omitted.
pub async fn update_with_options(storage: &mut StorageClient, group: &str, version: &str, resource: &str, namespace: Option<&str>, name: &str, body: &Value, dry_run: bool) -> Result<UpdateOutcome, Error> {
    let Some(resolved) = resolve_resource(storage, group, version, resource).await? else {
        return Ok(UpdateOutcome::UnknownResource);
    };
    let kind = resolved.kind.clone();

    let key = keys::object_key(group, resource, namespace, name);
    let existing_resp = storage.range(RangeRequest { key: key.clone().into_bytes(), ..Default::default() }).await?;
    let Some(existing_kv) = existing_resp.kvs.into_iter().next() else {
        return Ok(UpdateOutcome::ObjectNotFound);
    };
    let existing_object = decrypt_and_decode(storage, group, resource, &existing_kv.key, &existing_kv.value)?;

    if let (Some(ns), Some(body_ns)) = (namespace, body.pointer("/metadata/namespace").and_then(Value::as_str)) {
        if !body_ns.is_empty() && body_ns != ns {
            return Ok(UpdateOutcome::NamespaceMismatch);
        }
    }

    // Compared numerically, not as strings — resourceVersion is an
    // opaque string to a real client, but this build's own encoding of
    // it is always the decimal MVCC revision, so parsing avoids any
    // formatting-mismatch false negative (leading zeros, etc.).
    let Some(submitted_rv) = body.pointer("/metadata/resourceVersion").and_then(Value::as_str).and_then(|s| s.parse::<i64>().ok()) else {
        return Ok(UpdateOutcome::MissingResourceVersion);
    };
    if submitted_rv != existing_kv.mod_revision {
        return Ok(UpdateOutcome::Conflict);
    }

    // Group K: same pruning `create` runs, same order (before validation/
    // defaulting).
    let pruned_body;
    let body: &Value = match &resolved.open_api_schema {
        Some(open_api_schema) => {
            pruned_body = apiextensions::schema_pruning::prune(open_api_schema, body);
            &pruned_body
        }
        None => body,
    };

    let mut violations: Vec<String> = match (resolved.schema, &resolved.open_api_schema) {
        (Some(schema), _) => {
            let mut v: Vec<String> = validation::validate_required(schema, body).into_iter().map(|m| format!("{}: Required value", m.path)).collect();
            v.extend(validation::validate_types(schema, body).into_iter().map(|t| format!("{}: expected type {}, got {}", t.path, t.expected, t.actual_kind)));
            v
        }
        // Group K: same scope `create`'s own CRD branch runs.
        (None, Some(open_api_schema)) => {
            let mut v: Vec<String> = apiextensions::schema_validation::validate_required(open_api_schema, body).into_iter().map(|m| format!("{}: Required value", m.path)).collect();
            v.extend(apiextensions::schema_validation::validate_types(open_api_schema, body).into_iter().map(|t| format!("{}: expected type {}, got {}", t.path, t.expected, t.actual_kind)));
            v.extend(apiextensions::schema_validation::validate_constraints(open_api_schema, body));
            v
        }
        (None, None) => Vec::new(),
    };
    violations.extend(name_format_violations(group, resource, name).into_iter().map(|e| format!("metadata.name: {e}")));
    // Group K / CEL Phase 3: same real static cost check `create`'s own
    // CRD branch runs.
    if group == "apiextensions.k8s.io" && resource == "customresourcedefinitions" {
        violations.extend(apiextensions::cel_validations::validate_crd_cel_costs(body));
    }
    if !violations.is_empty() {
        return Ok(UpdateOutcome::Invalid(violations));
    }

    let object = match (resolved.schema, &resolved.open_api_schema) {
        (Some(schema), _) => defaulting::apply_defaults(schema, body),
        (None, Some(open_api_schema)) => apiextensions::schema_defaults::apply_defaults(open_api_schema, body),
        (None, None) => body.clone(),
    };

    // CEL Phase 4: same real rule evaluation `create`'s own CRD branch
    // runs, `old_value: Some(&existing_object)` this time — real
    // upstream's own `oldSelf` binding is exactly the object as it was
    // immediately before this update.
    if let Some(open_api_schema) = &resolved.open_api_schema {
        let rule_violations = apiextensions::cel_evaluate::validate_object(open_api_schema, &object, Some(&existing_object));
        if !rule_violations.is_empty() {
            return Ok(UpdateOutcome::Invalid(rule_violations.into_iter().map(|v| v.to_string()).collect()));
        }
    }

    persist_update(storage, resolved.schema, &kind, group, version, resource, key, &existing_kv, &existing_object, namespace, object, dry_run).await
}

/// Real upstream's generic status subresource (`GenericStatusREST`,
/// `k8s.io/apiserver/pkg/registry/generic/registry/store.go`'s own
/// `StatusREST`): a `PUT` through `<resource>/status` only ever changes
/// the object's `status` field — every other top-level field on the
/// submitted body (`spec`, most of `metadata`) is ignored, the existing
/// object's own spec/metadata survives untouched apart from the same
/// `creationTimestamp`/`uid` immutability [`persist_update`] already
/// enforces for a plain `update`. Same real optimistic concurrency as
/// `update` (submitted `metadata.resourceVersion` must match).
///
/// **Named, honest scope narrowing**: this build runs no
/// structural/type validation on the status write at all (real
/// upstream's own per-type status strategies — e.g. Pod's
/// `ValidatePodStatusUpdate` — are genuinely hand-written Go with no
/// generic table to derive them from, the same "no vendored enum
/// constraints" finding that already scoped `scheme::validation` down
/// elsewhere), and the namespace-mismatch check `update` runs against
/// the body is skipped (moot here — the body's own `metadata`/`spec` are
/// never read for anything but `resourceVersion`). [`patch_status`] is
/// this function's `PATCH` counterpart.
///
/// A CRD-defined resource whose matched version never declared
/// `subresources.status` has no `status` subresource at all — real
/// upstream doesn't even install this route for such a version — so
/// this returns `UnknownResource` (a real `404`) rather than silently
/// serving a status write real upstream itself would refuse. Every
/// built-in resource this crate resolves through the static table is
/// unaffected: `resolve_resource` always reports `true` for one, the
/// same "not modeled per-type yet" scope this crate's own discovery
/// already has for built-in subresources generally.
pub async fn update_status(storage: &mut StorageClient, group: &str, version: &str, resource: &str, namespace: Option<&str>, name: &str, body: &Value) -> Result<UpdateOutcome, Error> {
    let Some(resolved) = resolve_resource(storage, group, version, resource).await? else {
        return Ok(UpdateOutcome::UnknownResource);
    };
    if !resolved.has_status_subresource {
        return Ok(UpdateOutcome::UnknownResource);
    }
    let kind = resolved.kind.clone();

    let key = keys::object_key(group, resource, namespace, name);
    let existing_resp = storage.range(RangeRequest { key: key.clone().into_bytes(), ..Default::default() }).await?;
    let Some(existing_kv) = existing_resp.kvs.into_iter().next() else {
        return Ok(UpdateOutcome::ObjectNotFound);
    };
    let existing_object = decrypt_and_decode(storage, group, resource, &existing_kv.key, &existing_kv.value)?;

    let Some(submitted_rv) = body.pointer("/metadata/resourceVersion").and_then(Value::as_str).and_then(|s| s.parse::<i64>().ok()) else {
        return Ok(UpdateOutcome::MissingResourceVersion);
    };
    if submitted_rv != existing_kv.mod_revision {
        return Ok(UpdateOutcome::Conflict);
    }

    let mut object = existing_object.clone();
    match body.get("status") {
        Some(status) => object["status"] = status.clone(),
        None => {
            if let Some(map) = object.as_object_mut() {
                map.remove("status");
            }
        }
    }

    persist_update(storage, resolved.schema, &kind, group, version, resource, key, &existing_kv, &existing_object, namespace, object, false).await
}

/// The tail [`update`] and [`patch`] share once each has its own
/// candidate object in hand (a defaulted submitted body for `update`, a
/// patch-applied one for `patch`): preserve `creationTimestamp`/`uid`
/// from the existing object (real upstream treats both as immutable
/// after creation, regardless of what the caller's patch/body touched),
/// stamp the namespace, then a real optimistic-concurrency `Txn`
/// compared against the exact revision both callers already read —
/// a concurrent write between that read and this write loses the race
/// and gets a real `Conflict`, not a silent overwrite.
async fn persist_update(
    storage: &mut StorageClient,
    schema: Option<&str>,
    kind: &str,
    group: &str,
    version: &str,
    resource: &str,
    key: String,
    existing_kv: &mvccpb::KeyValue,
    existing_object: &Value,
    namespace: Option<&str>,
    mut object: Value,
    dry_run: bool,
) -> Result<UpdateOutcome, Error> {
    for field in ["creationTimestamp", "uid"] {
        if let Some(existing_value) = existing_object.pointer(&format!("/metadata/{field}")).cloned() {
            set_metadata_field(&mut object, field, existing_value);
        }
    }
    if let Some(ns) = namespace {
        set_metadata_field(&mut object, "namespace", Value::String(ns.to_string()));
    }

    object = crate::scheme::conversion::to_version(group, version, kind, object);

    if dry_run {
        return Ok(UpdateOutcome::Updated(object));
    }

    let api_version = if group.is_empty() { version.to_string() } else { format!("{group}/{version}") };
    let object_bytes = match schema {
        Some(schema) => protobuf::encode_message(schema, &object)?,
        None => serde_json::to_vec(&object).map_err(protobuf::Error::Json)?,
    };
    let envelope = protobuf::wrap_unknown(&api_version, kind, &object_bytes);

    let compare = pb::Compare {
        key: key.clone().into_bytes(),
        result: pb::compare::CompareResult::Equal as i32,
        target: pb::compare::CompareTarget::Mod as i32,
        target_union: Some(pb::compare::TargetUnion::ModRevision(existing_kv.mod_revision)),
        range_end: Vec::new(),
    };
    let envelope = encrypt_for_storage(storage, group, resource, key.as_bytes(), &envelope)?;
    let put = pb::PutRequest { key: key.into_bytes(), value: envelope, ..Default::default() };
    let txn = pb::TxnRequest {
        compare: vec![compare],
        success: vec![pb::RequestOp { request: Some(pb::request_op::Request::RequestPut(put)) }],
        failure: vec![],
    };
    let resp = storage.txn(txn).await?;
    if !resp.succeeded {
        // Lost the race: something else wrote to this key between our
        // read above and this write.
        return Ok(UpdateOutcome::Conflict);
    }

    let revision = resp.header.map(|h| h.revision).unwrap_or(0);
    set_metadata_field(&mut object, "resourceVersion", Value::String(revision.to_string()));
    Ok(UpdateOutcome::Updated(object))
}

/// The three real patch media types this build understands, and which
/// `patch::*` module applies each. The request handler separately applies
/// Kubernetes' default strategy when a request has no `Content-Type`:
/// strategic merge for built-in resources and merge patch for CRDs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PatchKind {
    Json,
    Merge,
    StrategicMerge,
}

/// Real upstream's own three patch `Content-Type` media types
/// (`k8s.io/apimachinery/pkg/types`): `application/json-patch+json`
/// (RFC 6902), `application/merge-patch+json` (RFC 7386),
/// `application/strategic-merge-patch+json` (k8s-specific). Server-Side
/// Apply's own `application/apply-patch+yaml` isn't recognized — Group
/// G's own doc comment already names SSA/managedFields as not yet
/// landed.
pub fn patch_kind_for_content_type(content_type: &str) -> Option<PatchKind> {
    match content_type.split(';').next().unwrap_or("").trim() {
        "application/json-patch+json" => Some(PatchKind::Json),
        "application/merge-patch+json" => Some(PatchKind::Merge),
        "application/strategic-merge-patch+json" => Some(PatchKind::StrategicMerge),
        _ => None,
    }
}

/// Kubernetes' default patch strategy when a request omits `Content-Type`.
/// Built-in resources have compiled schemas and therefore use strategic
/// merge; CRD-defined resources use JSON merge patch because they do not
/// have the generated strategic-merge metadata used by built-ins.
pub fn default_patch_kind(is_crd: bool) -> PatchKind {
    if is_crd { PatchKind::Merge } else { PatchKind::StrategicMerge }
}

/// Resolves the resource and returns the default patch strategy for a
/// request with no `Content-Type`. `None` means the URL names no resource
/// this server knows about, so the listener can preserve its normal 404
/// response rather than reporting a media-type error.
pub async fn default_patch_kind_for_request(storage: &mut StorageClient, group: &str, version: &str, resource: &str) -> Result<Option<PatchKind>, Error> {
    Ok(resolve_resource(storage, group, version, resource).await?.map(|resolved| default_patch_kind(resolved.schema.is_none())))
}

/// Apply a CEL `MutatingAdmissionPolicy` apply configuration to an admission
/// object. Apply configurations use the same strategic-merge rules as the
/// server's strategic-merge PATCH path; built-ins use their generated schema
/// and CRDs use their runtime OpenAPI schema. A resource without either
/// schema falls back to JSON merge semantics, which preserves the generic
/// server's behavior for schema-less resources.
pub async fn apply_admission_configuration(storage: &mut StorageClient, group: &str, version: &str, resource: &str, existing: &Value, configuration: &Value) -> Result<Value, Error> {
    let Some(resolved) = resolve_resource(storage, group, version, resource).await? else {
        return Err(Error::UnknownResource);
    };
    Ok(match (resolved.schema, resolved.open_api_schema.as_ref()) {
        (Some(schema), _) => crate::patch::strategic_merge::apply(schema, existing, configuration),
        (None, Some(schema)) => apiextensions::schema_strategic_merge::apply(schema, existing, configuration),
        (None, None) => {
            let mut object = existing.clone();
            crate::patch::merge_patch::apply(&mut object, configuration);
            object
        }
    })
}

/// The context [`patch_prepare`] hands back to [`patch_persist`] once a
/// patch has been applied but before it's validated/persisted — enough
/// to run Group J admission against the real candidate object in
/// between (`server::listener`'s own `PATCH` branch does exactly this
/// for `LimitRanger`), without re-fetching or re-applying the patch a
/// second time.
#[derive(Debug)]
pub struct PatchContext {
    /// `None` for a CRD-defined resource — see [`apply_patch`]'s own doc
    /// comment for what that rules out (`strategic-merge-patch`) and
    /// what it doesn't (`JSON Patch`/`Merge Patch`, and
    /// [`patch_persist`]'s own schema-driven defaulting, which falls
    /// back to `open_api_schema` in exactly this case).
    schema: Option<&'static str>,
    open_api_schema: Option<Value>,
    kind: String,
    key: String,
    existing_kv: mvccpb::KeyValue,
    existing_object: Value,
}

#[derive(Debug)]
pub enum PatchPrepareOutcome {
    /// The patch applied cleanly; `candidate` is the resulting object,
    /// not yet validated/defaulted/persisted.
    Ready(Value, PatchContext),
    UnknownResource,
    ObjectNotFound,
    /// The patch itself couldn't be applied (a JSON Patch `test` op
    /// failure, or a malformed patch document).
    Invalid(Vec<String>),
}

/// Applies one of this build's three real patch kinds
/// ([`crate::patch::json_patch`]/[`crate::patch::merge_patch`]/
/// [`crate::patch::strategic_merge`], all landed in Group G) to
/// `existing`. Shared by [`patch_prepare`] (patches the whole object)
/// and [`patch_status`] (patches the whole object too — real upstream's
/// own subresource PATCH semantics: the patch document can reference
/// any path, only the final write is restricted to `.status` — the
/// restriction happens at persist time, not by scoping what the patch
/// itself can touch).
///
/// `schema` is `None` for a CRD-defined resource — `JSON Patch`/`Merge
/// Patch` need no schema at all and work identically either way;
/// `strategic-merge-patch` uses `open_api_schema` instead in that case
/// (`apiextensions::schema_strategic_merge`, the runtime-schema sibling
/// of `crate::patch::strategic_merge`'s own compiled-`ref_schema` walk).
/// `open_api_schema` is `None` too only for a CRD version whose own
/// document carries no schema at all (a real, if unusual, case this
/// build's own read path already tolerates elsewhere — a malformed/
/// legacy document, `apiextensions::registry::CrdResource`'s own doc
/// comment) — a `strategic-merge-patch` against one has no schema of any
/// kind to interpret, a real `Invalid`, not a panic.
fn apply_patch(kind_of_patch: PatchKind, schema: Option<&str>, open_api_schema: Option<&Value>, existing: &Value, patch_doc: &Value) -> Result<Value, String> {
    match kind_of_patch {
        PatchKind::Json => {
            let mut object = existing.clone();
            if crate::patch::json_patch::apply(&mut object, patch_doc).is_err() {
                return Err("the submitted JSON Patch could not be applied".to_string());
            }
            Ok(object)
        }
        PatchKind::Merge => {
            let mut object = existing.clone();
            crate::patch::merge_patch::apply(&mut object, patch_doc);
            Ok(object)
        }
        PatchKind::StrategicMerge => match (schema, open_api_schema) {
            (Some(schema), _) => Ok(crate::patch::strategic_merge::apply(schema, existing, patch_doc)),
            (None, Some(open_api_schema)) => Ok(apiextensions::schema_strategic_merge::apply(open_api_schema, existing, patch_doc)),
            (None, None) => Err("strategic-merge-patch: this resource has no known schema to interpret x-kubernetes-list-type/-list-map-keys against".to_string()),
        },
    }
}

/// Reads the current object and applies one of this build's three real
/// patch kinds to it — the "prepare" half of [`patch`], split out so a
/// caller (`server::listener`) can run Group J admission against the
/// real candidate object before committing to [`patch_persist`].
pub async fn patch_prepare(storage: &mut StorageClient, group: &str, version: &str, resource: &str, namespace: Option<&str>, name: &str, kind_of_patch: PatchKind, patch_doc: &Value) -> Result<PatchPrepareOutcome, Error> {
    let Some(resolved) = resolve_resource(storage, group, version, resource).await? else {
        return Ok(PatchPrepareOutcome::UnknownResource);
    };

    let key = keys::object_key(group, resource, namespace, name);
    let existing_resp = storage.range(RangeRequest { key: key.clone().into_bytes(), ..Default::default() }).await?;
    let Some(existing_kv) = existing_resp.kvs.into_iter().next() else {
        return Ok(PatchPrepareOutcome::ObjectNotFound);
    };
    let existing_object = decrypt_and_decode(storage, group, resource, &existing_kv.key, &existing_kv.value)?;

    let patched = match apply_patch(kind_of_patch, resolved.schema, resolved.open_api_schema.as_ref(), &existing_object, patch_doc) {
        Ok(object) => object,
        Err(msg) => return Ok(PatchPrepareOutcome::Invalid(vec![msg])),
    };

    Ok(PatchPrepareOutcome::Ready(
        patched,
        PatchContext { schema: resolved.schema, open_api_schema: resolved.open_api_schema, kind: resolved.kind, key, existing_kv, existing_object },
    ))
}

/// The "persist" half of [`patch`]: validates/defaults `candidate` (the
/// object [`patch_prepare`] produced, possibly further mutated by
/// admission in between) and writes it with the same real optimistic
/// concurrency [`update`] uses (`Txn`-compared-against-`ModRevision`,
/// via the shared [`persist_update`] tail) — no client-submitted
/// `resourceVersion` needed, since the object being patched *is* the one
/// [`patch_prepare`] already read.
pub async fn patch_persist(storage: &mut StorageClient, group: &str, version: &str, resource: &str, namespace: Option<&str>, name: &str, context: PatchContext, candidate: Value) -> Result<UpdateOutcome, Error> {
    // Group K: same pruning `create`/`update` run, same order (before
    // validation/defaulting) — `candidate` is already owned, so this
    // just reassigns it rather than needing the borrow-juggling
    // `create`/`update` need for their own `&Value` parameter.
    let candidate = match &context.open_api_schema {
        Some(open_api_schema) => apiextensions::schema_pruning::prune(open_api_schema, &candidate),
        None => candidate,
    };

    let mut violations: Vec<String> = match (context.schema, &context.open_api_schema) {
        (Some(schema), _) => {
            let mut v: Vec<String> = validation::validate_required(schema, &candidate).into_iter().map(|m| format!("{}: Required value", m.path)).collect();
            v.extend(validation::validate_types(schema, &candidate).into_iter().map(|t| format!("{}: expected type {}, got {}", t.path, t.expected, t.actual_kind)));
            v
        }
        // Group K: same scope `create`'s own CRD branch runs.
        (None, Some(open_api_schema)) => {
            let mut v: Vec<String> = apiextensions::schema_validation::validate_required(open_api_schema, &candidate).into_iter().map(|m| format!("{}: Required value", m.path)).collect();
            v.extend(apiextensions::schema_validation::validate_types(open_api_schema, &candidate).into_iter().map(|t| format!("{}: expected type {}, got {}", t.path, t.expected, t.actual_kind)));
            v.extend(apiextensions::schema_validation::validate_constraints(open_api_schema, &candidate));
            v
        }
        (None, None) => Vec::new(),
    };
    violations.extend(name_format_violations(group, resource, name).into_iter().map(|e| format!("metadata.name: {e}")));
    if !violations.is_empty() {
        return Ok(UpdateOutcome::Invalid(violations));
    }

    let object = match (context.schema, &context.open_api_schema) {
        (Some(schema), _) => defaulting::apply_defaults(schema, &candidate),
        (None, Some(open_api_schema)) => apiextensions::schema_defaults::apply_defaults(open_api_schema, &candidate),
        (None, None) => candidate,
    };

    // CEL Phase 4: same real rule evaluation `create`/`update` both run.
    if let Some(open_api_schema) = &context.open_api_schema {
        let rule_violations = apiextensions::cel_evaluate::validate_object(open_api_schema, &object, Some(&context.existing_object));
        if !rule_violations.is_empty() {
            return Ok(UpdateOutcome::Invalid(rule_violations.into_iter().map(|v| v.to_string()).collect()));
        }
    }

    persist_update(storage, context.schema, &context.kind, group, version, resource, context.key, &context.existing_kv, &context.existing_object, namespace, object, false).await
}

/// `PATCH .../status` — the patch counterpart to [`update_status`],
/// closing the "PUT-only" gap `docs/APISERVER.md` named for it. Applies
/// the patch to the whole existing object (same
/// [`apply_patch`] `patch_prepare` uses — real upstream's own subresource
/// PATCH semantics let the patch document reference any path), then
/// takes only the result's own `.status` field and merges it onto the
/// existing object exactly the way `update_status` does, so a
/// `strategic-merge-patch+json` `{"status": {...}}` document behaves the
/// same whether it arrives via `PUT` (full replace) or `PATCH` (merged).
/// No client-submitted `resourceVersion` needed, same as `patch_persist`.
/// Same scope narrowing `update_status` already named: no structural
/// validation, no Group J admission — and the same
/// `subresources.status`-must-be-declared gate for a CRD-defined
/// resource (`update_status`'s own doc comment covers why).
pub async fn patch_status(storage: &mut StorageClient, group: &str, version: &str, resource: &str, namespace: Option<&str>, name: &str, kind_of_patch: PatchKind, patch_doc: &Value) -> Result<UpdateOutcome, Error> {
    let Some(resolved) = resolve_resource(storage, group, version, resource).await? else {
        return Ok(UpdateOutcome::UnknownResource);
    };
    if !resolved.has_status_subresource {
        return Ok(UpdateOutcome::UnknownResource);
    }

    let key = keys::object_key(group, resource, namespace, name);
    let existing_resp = storage.range(RangeRequest { key: key.clone().into_bytes(), ..Default::default() }).await?;
    let Some(existing_kv) = existing_resp.kvs.into_iter().next() else {
        return Ok(UpdateOutcome::ObjectNotFound);
    };
    let existing_object = decrypt_and_decode(storage, group, resource, &existing_kv.key, &existing_kv.value)?;

    let patched = match apply_patch(kind_of_patch, resolved.schema, resolved.open_api_schema.as_ref(), &existing_object, patch_doc) {
        Ok(object) => object,
        Err(msg) => return Ok(UpdateOutcome::Invalid(vec![msg])),
    };

    let mut object = existing_object.clone();
    match patched.get("status") {
        Some(status) => object["status"] = status.clone(),
        None => {
            if let Some(map) = object.as_object_mut() {
                map.remove("status");
            }
        }
    }

    persist_update(storage, resolved.schema, &resolved.kind, group, version, resource, key, &existing_kv, &existing_object, namespace, object, false).await
}

/// Convenience wrapper combining [`patch_prepare`] and [`patch_persist`]
/// with no admission step in between — what `server::rest::patch` used
/// to do as one function before the split; kept for any caller that
/// doesn't need to run admission in the middle (this crate's own tests).
pub async fn patch(storage: &mut StorageClient, group: &str, version: &str, resource: &str, namespace: Option<&str>, name: &str, kind_of_patch: PatchKind, patch_doc: &Value) -> Result<UpdateOutcome, Error> {
    match patch_prepare(storage, group, version, resource, namespace, name, kind_of_patch, patch_doc).await? {
        PatchPrepareOutcome::Ready(candidate, context) => patch_persist(storage, group, version, resource, namespace, name, context, candidate).await,
        PatchPrepareOutcome::UnknownResource => Ok(UpdateOutcome::UnknownResource),
        PatchPrepareOutcome::ObjectNotFound => Ok(UpdateOutcome::ObjectNotFound),
        PatchPrepareOutcome::Invalid(v) => Ok(UpdateOutcome::Invalid(v)),
    }
}

/// The outcome of a Server-Side Apply request ([`server_side_apply`]).
#[derive(Debug, PartialEq)]
pub enum ApplyOutcome {
    /// The object as written, `metadata.managedFields` rebuilt to
    /// reflect this apply.
    Applied(Value),
    /// The merged-and-pruned result was byte-for-byte identical to the
    /// object already stored — nothing written, real upstream's own
    /// no-op contract (`crate::patch::updater::Applied::object`'s own
    /// doc comment). The caller still gets a real `200` with the
    /// current object, matching real upstream's own behavior.
    NoOp(Value),
    UnknownResource,
    /// Another manager owns a field this apply is changing — real
    /// upstream's own `409 Conflict`, not raised unless `force` is
    /// false.
    Conflict(Vec<crate::patch::updater::Conflict>),
    Invalid(Vec<String>),
}

/// Server-Side Apply (`PATCH` with `Content-Type: application/apply-
/// patch+yaml`) — real upstream's `merge.Updater.Apply`, wired to real
/// storage (`crate::patch::updater::apply`,
/// `crate::patch::managed_fields`). `config` is the apply configuration,
/// already decoded from the request body by the caller (YAML or JSON —
/// real upstream accepts either for this content type, and this crate's
/// existing content negotiation already handles both for every other
/// verb).
///
/// Handles both real cases: an already-existing object (reads its
/// stored `managedFields`, runs `updater::apply` against it, persists
/// with the same optimistic-concurrency `Txn` every other write verb
/// uses) and **create-on-apply** (no object exists at this key yet —
/// real upstream's own Apply can create one, `liveObject` starting
/// empty; this branch runs the identical `updater::apply` orchestration
/// against an empty `live`, then persists with the same
/// create-only-if-absent `Txn` idiom `create`'s own doc comment names,
/// rather than `persist_update`'s update-if-matches one).
///
/// Named `server_side_apply`, not `apply_patch` — that name is already
/// this module's own private helper for the three ordinary patch kinds
/// (`json_patch`/`merge_patch`/`strategic_merge`) just above; this is a
/// wholly different real orchestration, not a fourth branch of that one.
///
/// A convenience wrapper combining [`apply_prepare`] and
/// [`apply_persist`] with no admission step in between — the same shape
/// [`patch`] is to [`patch_prepare`]/[`patch_persist`]. `server::
/// listener`'s own real request handler calls the two halves directly
/// instead, so it can run Group J's `LimitRanger` PVC check against the
/// real candidate object in between, the same way it already does for
/// the three-patch-kind `PATCH` path.
pub async fn server_side_apply(storage: &mut StorageClient, group: &str, version: &str, resource: &str, namespace: Option<&str>, name: &str, manager: &str, force: bool, config: &Value) -> Result<ApplyOutcome, Error> {
    match apply_prepare(storage, group, version, resource, namespace, name, manager, force, config).await? {
        ApplyPrepareOutcome::Ready(candidate, context) => apply_persist(storage, group, version, resource, namespace, context, candidate).await,
        ApplyPrepareOutcome::UnknownResource => Ok(ApplyOutcome::UnknownResource),
        ApplyPrepareOutcome::Conflict(c) => Ok(ApplyOutcome::Conflict(c)),
        ApplyPrepareOutcome::Invalid(v) => Ok(ApplyOutcome::Invalid(v)),
        ApplyPrepareOutcome::NoOp(v) => Ok(ApplyOutcome::NoOp(v)),
    }
}

/// The context [`apply_prepare`] hands back to [`apply_persist`] once the
/// merged, pruned, conflict-checked, validated, defaulted candidate is
/// ready — enough for a caller (`server::listener`) to run Group J
/// admission (`LimitRanger`'s own PVC check) against the real candidate
/// in between, the same split [`PatchContext`] already exists for.
#[derive(Debug)]
pub struct ApplyContext {
    /// `Some` for a built-in compiled schema and `None` for a CRD whose
    /// runtime schema has already been consumed during preparation.
    schema: Option<&'static str>,
    kind: String,
    key: String,
    /// `Some((existing_kv, live))` for an update-on-apply (persisted via
    /// [`persist_update`]'s update-if-matches `Txn`); `None` for
    /// create-on-apply (persisted via the same create-only-if-absent
    /// `Txn` idiom [`create`]'s own doc comment names).
    existing: Option<(mvccpb::KeyValue, Value)>,
}

#[derive(Debug)]
pub enum ApplyPrepareOutcome {
    Ready(Value, ApplyContext),
    UnknownResource,
    Conflict(Vec<crate::patch::updater::Conflict>),
    Invalid(Vec<String>),
    /// The merged-and-pruned result was identical to what's already
    /// stored (or, for create-on-apply, `config` was itself empty) —
    /// nothing to persist, `Value` is what to return to the caller.
    NoOp(Value),
}

/// The "prepare" half of [`server_side_apply`]: resolves the resource,
/// reads the current object (if any), runs the real `updater::apply`
/// orchestration, rebuilds `managedFields`, and validates/defaults the
/// result — everything short of the actual `Txn` write, so a caller can
/// run Group J admission against the real candidate object in between
/// (`server::listener`'s own `PATCH` branch does exactly this for
/// `LimitRanger`, mirroring how [`patch_prepare`]/[`patch_persist`]
/// already split for the same reason).
pub async fn apply_prepare(storage: &mut StorageClient, group: &str, version: &str, resource: &str, namespace: Option<&str>, name: &str, manager: &str, force: bool, config: &Value) -> Result<ApplyPrepareOutcome, Error> {
    let Some(resolved) = resolve_resource(storage, group, version, resource).await? else {
        return Ok(ApplyPrepareOutcome::UnknownResource);
    };
    let Some(schema) = resolved.schema else {
        return apply_prepare_crd(storage, group, version, resource, namespace, name, manager, force, config, resolved.kind, resolved.open_api_schema.unwrap_or(Value::Null)).await;
    };

    let key = keys::object_key(group, resource, namespace, name);
    let existing_resp = storage.range(RangeRequest { key: key.clone().into_bytes(), ..Default::default() }).await?;
    let api_version = if group.is_empty() { version.to_string() } else { format!("{group}/{version}") };

    let Some(existing_kv) = existing_resp.kvs.into_iter().next() else {
        // Create-on-apply: real upstream's own Apply can create a
        // brand-new object when none exists yet (`liveObject` starts
        // empty) -- `updater::apply` already supports that structurally.
        let live = json!({});
        let no_prior_managers = std::collections::BTreeMap::new();
        let applied = match crate::patch::updater::apply(schema, &live, config, &no_prior_managers, manager, force) {
            Ok(a) => a,
            // Unreachable in practice (an empty prior-managers map can
            // never conflict), kept real rather than `unreachable!()` --
            // `updater::apply`'s own contract doesn't promise this.
            Err(conflicts) => return Ok(ApplyPrepareOutcome::Conflict(conflicts)),
        };
        let Some(mut object) = applied.object else {
            // The apply configuration was itself empty (merges to `{}`)
            // -- nothing real to create.
            return Ok(ApplyPrepareOutcome::NoOp(live));
        };

        set_metadata_field(&mut object, "creationTimestamp", Value::String(now_rfc3339()));
        set_metadata_field(&mut object, "uid", Value::String(uuid::Uuid::new_v4().to_string()));
        // The object's identity comes from the URL, same as every other
        // verb here (`persist_update` forces `namespace` from the URL
        // the same unconditional way) -- not from whatever `config`'s
        // own body happened to say.
        set_metadata_field(&mut object, "name", Value::String(name.to_string()));
        if let Some(ns) = namespace {
            set_metadata_field(&mut object, "namespace", Value::String(ns.to_string()));
        }
        let rebuilt = crate::patch::managed_fields::rebuild_managed_fields(&[], &applied.managers, manager, "", "Apply", &api_version, Some(&now_rfc3339()));
        set_metadata_field(&mut object, "managedFields", crate::patch::managed_fields::render_managed_fields(&rebuilt));

        let mut violations: Vec<String> = validation::validate_required(schema, &object).into_iter().map(|m| format!("{}: Required value", m.path)).collect();
        violations.extend(validation::validate_types(schema, &object).into_iter().map(|t| format!("{}: expected type {}, got {}", t.path, t.expected, t.actual_kind)));
        violations.extend(name_format_violations(group, resource, name).into_iter().map(|e| format!("metadata.name: {e}")));
        if !violations.is_empty() {
            return Ok(ApplyPrepareOutcome::Invalid(violations));
        }
        let object = defaulting::apply_defaults(schema, &object);

        return Ok(ApplyPrepareOutcome::Ready(object, ApplyContext { schema: Some(schema), kind: resolved.kind, key, existing: None }));
    };

    let live = decrypt_and_decode(storage, group, resource, &existing_kv.key, &existing_kv.value)?;

    let stored_managed_fields = live.pointer("/metadata/managedFields").cloned().unwrap_or_else(|| Value::Array(Vec::new()));
    // A stored `managedFields` this crate can't parse (malformed, or an
    // entry with a `fieldsType` this crate doesn't understand — see
    // `managed_fields::parse_managed_fields`'s own doc comment) degrades
    // to "no prior bookkeeping" rather than failing the whole apply: the
    // object itself is still perfectly real and applicable, only the
    // ownership history is unrecoverable.
    let entries = crate::patch::managed_fields::parse_managed_fields(&stored_managed_fields).unwrap_or_default();
    let managers = crate::patch::managed_fields::to_managers_map(&entries);

    let applied = match crate::patch::updater::apply(schema, &live, config, &managers, manager, force) {
        Ok(a) => a,
        Err(conflicts) => return Ok(ApplyPrepareOutcome::Conflict(conflicts)),
    };

    let Some(mut object) = applied.object else {
        return Ok(ApplyPrepareOutcome::NoOp(live));
    };

    let rebuilt = crate::patch::managed_fields::rebuild_managed_fields(&entries, &applied.managers, manager, "", "Apply", &api_version, Some(&now_rfc3339()));
    set_metadata_field(&mut object, "managedFields", crate::patch::managed_fields::render_managed_fields(&rebuilt));

    let mut violations: Vec<String> = validation::validate_required(schema, &object).into_iter().map(|m| format!("{}: Required value", m.path)).collect();
    violations.extend(validation::validate_types(schema, &object).into_iter().map(|t| format!("{}: expected type {}, got {}", t.path, t.expected, t.actual_kind)));
    violations.extend(name_format_violations(group, resource, name).into_iter().map(|e| format!("metadata.name: {e}")));
    if !violations.is_empty() {
        return Ok(ApplyPrepareOutcome::Invalid(violations));
    }
    let object = defaulting::apply_defaults(schema, &object);

    Ok(ApplyPrepareOutcome::Ready(object, ApplyContext { schema: Some(schema), kind: resolved.kind, key, existing: Some((existing_kv, live)) }))
}

/// The runtime-schema sibling of [`apply_prepare`] for a CRD-defined
/// resource. Its storage envelope is JSON rather than protobuf, but the
/// optimistic-concurrency and managed-fields protocol is identical to the
/// built-in path above.
async fn apply_prepare_crd(
    storage: &mut StorageClient,
    group: &str,
    version: &str,
    resource: &str,
    namespace: Option<&str>,
    name: &str,
    manager: &str,
    force: bool,
    config: &Value,
    kind: String,
    schema: Value,
) -> Result<ApplyPrepareOutcome, Error> {
    let key = keys::object_key(group, resource, namespace, name);
    let existing_resp = storage.range(RangeRequest { key: key.clone().into_bytes(), ..Default::default() }).await?;
    let (live, existing_kv) = match existing_resp.kvs.into_iter().next() {
        Some(existing_kv) => (decrypt_and_decode(storage, group, resource, &existing_kv.key, &existing_kv.value)?, Some(existing_kv)),
        None => (json!({}), None),
    };
    let stored = live.pointer("/metadata/managedFields").cloned().unwrap_or_else(|| Value::Array(Vec::new()));
    let entries = crate::patch::managed_fields::parse_managed_fields(&stored).unwrap_or_default();
    let managers = crate::patch::managed_fields::to_managers_map(&entries);
    let applied = match apiextensions::schema_apply::apply(&schema, &live, config, &managers, manager, force) {
        Ok(applied) => applied,
        Err(conflicts) => {
            return Ok(ApplyPrepareOutcome::Conflict(
                conflicts
                    .into_iter()
                    .map(|conflict| crate::patch::updater::Conflict { manager: conflict.manager, fields: conflict.fields })
                    .collect(),
            ));
        }
    };
    let Some(mut object) = applied.object else {
        return Ok(ApplyPrepareOutcome::NoOp(live));
    };

    if existing_kv.is_none() {
        set_metadata_field(&mut object, "creationTimestamp", Value::String(now_rfc3339()));
        set_metadata_field(&mut object, "uid", Value::String(uuid::Uuid::new_v4().to_string()));
    }
    set_metadata_field(&mut object, "name", Value::String(name.to_string()));
    if let Some(namespace) = namespace {
        set_metadata_field(&mut object, "namespace", Value::String(namespace.to_string()));
    }
    let api_version = if group.is_empty() { version.to_string() } else { format!("{group}/{version}") };
    let rebuilt = crate::patch::managed_fields::rebuild_managed_fields(&entries, &applied.managers, manager, "", "Apply", &api_version, Some(&now_rfc3339()));
    set_metadata_field(&mut object, "managedFields", crate::patch::managed_fields::render_managed_fields(&rebuilt));

    let has_schema = !schema.is_null();
    let object = if has_schema { apiextensions::schema_pruning::prune(&schema, &object) } else { object };
    let mut violations: Vec<String> = if has_schema {
        apiextensions::schema_validation::validate_required(&schema, &object)
            .into_iter()
            .map(|violation| format!("{}: Required value", violation.path))
            .collect()
    } else {
        Vec::new()
    };
    if has_schema {
        violations.extend(
            apiextensions::schema_validation::validate_types(&schema, &object)
                .into_iter()
                .map(|violation| format!("{}: expected type {}, got {}", violation.path, violation.expected, violation.actual_kind)),
        );
        violations.extend(apiextensions::schema_validation::validate_constraints(&schema, &object));
    }
    violations.extend(name_format_violations(group, resource, name).into_iter().map(|error| format!("metadata.name: {error}")));
    if !violations.is_empty() {
        return Ok(ApplyPrepareOutcome::Invalid(violations));
    }
    let object = if has_schema { apiextensions::schema_defaults::apply_defaults(&schema, &object) } else { object };
    if has_schema && existing_kv.is_none() {
        let rule_violations = apiextensions::cel_evaluate::validate_object(&schema, &object, None);
        if !rule_violations.is_empty() {
            return Ok(ApplyPrepareOutcome::Invalid(rule_violations.into_iter().map(|violation| violation.to_string()).collect()));
        }
    } else if has_schema {
        let rule_violations = apiextensions::cel_evaluate::validate_object(&schema, &object, Some(&live));
        if !rule_violations.is_empty() {
            return Ok(ApplyPrepareOutcome::Invalid(rule_violations.into_iter().map(|violation| violation.to_string()).collect()));
        }
    }

    Ok(ApplyPrepareOutcome::Ready(object, ApplyContext { schema: None, kind, key, existing: existing_kv.map(|existing_kv| (existing_kv, live)) }))
}

/// The "persist" half of [`server_side_apply`]: writes `object` (the
/// candidate [`apply_prepare`] produced, possibly further mutated by
/// admission in between) with whichever real `Txn` idiom
/// [`ApplyContext::existing`] calls for.
pub async fn apply_persist(storage: &mut StorageClient, group: &str, version: &str, resource: &str, namespace: Option<&str>, context: ApplyContext, mut object: Value) -> Result<ApplyOutcome, Error> {
    let Some((existing_kv, live)) = context.existing else {
        let api_version = if group.is_empty() { version.to_string() } else { format!("{group}/{version}") };
        let object_bytes = match context.schema {
            Some(schema) => protobuf::encode_message(schema, &object)?,
            None => serde_json::to_vec(&object).map_err(protobuf::Error::Json)?,
        };
        let envelope = protobuf::wrap_unknown(&api_version, &context.kind, &object_bytes);
        let compare = pb::Compare {
            key: context.key.clone().into_bytes(),
            result: pb::compare::CompareResult::Equal as i32,
            target: pb::compare::CompareTarget::Mod as i32,
            target_union: Some(pb::compare::TargetUnion::ModRevision(0)),
            range_end: Vec::new(),
        };
        let envelope = encrypt_for_storage(storage, group, resource, context.key.as_bytes(), &envelope)?;
        let put = pb::PutRequest { key: context.key.into_bytes(), value: envelope, ..Default::default() };
        let txn = pb::TxnRequest {
            compare: vec![compare],
            success: vec![pb::RequestOp { request: Some(pb::request_op::Request::RequestPut(put)) }],
            failure: vec![],
        };
        let resp = storage.txn(txn).await?;
        if !resp.succeeded {
            // Lost the race: something else created this key between
            // `apply_prepare`'s own read and this write.
            return Ok(ApplyOutcome::Conflict(Vec::new()));
        }
        let revision = resp.header.map(|h| h.revision).unwrap_or(0);
        set_metadata_field(&mut object, "resourceVersion", Value::String(revision.to_string()));
        return Ok(ApplyOutcome::Applied(object));
    };

    match persist_update(storage, context.schema, &context.kind, group, version, resource, context.key, &existing_kv, &live, namespace, object, false).await? {
        UpdateOutcome::Updated(v) => Ok(ApplyOutcome::Applied(v)),
        // Lost the optimistic-concurrency race between `apply_prepare`'s
        // own read and this write -- a real, if rare, "retry and see
        // fresh conflicts" situation `updater::apply`'s own conflict
        // detection can't catch by itself, since it never re-reads
        // storage. Reported the same way as an ownership conflict (an
        // empty list, since no *manager* conflict was actually detected)
        // rather than inventing a third outcome variant this early
        // caller-side distinction doesn't otherwise need.
        UpdateOutcome::Conflict => Ok(ApplyOutcome::Conflict(Vec::new())),
        other => unreachable!("persist_update only ever returns Updated or Conflict for an already-decoded, already-validated object: {other:?}"),
    }
}

/// `scheme::name_format`'s validators, wired to the resources this crate
/// has actually verified a real per-type rule for
/// (`apimachinery/pkg/api/validation/generic.go`, confirmed directly):
/// `namespaces` (core group) uses `NameIsDNSLabel`
/// (`ValidateNamespaceName = NameIsDNSLabel`), `serviceaccounts` (core
/// group) uses `NameIsDNSSubdomain` (`ValidateServiceAccountName =
/// NameIsDNSSubdomain`). Every other `(group, resource)` returns no
/// violations at all — not because every other name is assumed valid,
/// but because this crate hasn't verified which real validator applies
/// to it yet; see `scheme::name_format`'s own doc comment for why that
/// mapping isn't a generically-derivable table. Extend this match one
/// verified entry at a time, the same way `scheme::defaulting`'s own
/// concrete case (`ContainerPort.protocol`) was landed and proven before
/// generalizing.
fn name_format_violations(group: &str, resource: &str, name: &str) -> Vec<String> {
    match (group, resource) {
        ("", "namespaces") => crate::scheme::name_format::is_dns1123_label(name),
        ("", "serviceaccounts") => crate::scheme::name_format::is_dns1123_subdomain(name),
        // `pkg/apis/core/validation/validation.go` (release-1.34, fetched
        // and grepped directly), each a literal `var Validate<Kind>Name =
        // apimachineryvalidation.NameIsDNSSubdomain` declaration: Pod,
        // ReplicationController, Node, LimitRange, ResourceQuota, Secret,
        // Endpoints, PersistentVolume, ConfigMap. All ten (including the
        // two already above) resolve to the core (`""`) group — confirmed
        // against the vendored `api__v1_openapi.json` `paths` table, not
        // assumed from this being the "core" validation file (some of its
        // other `var`s, e.g. `ValidatePriorityClassName`/
        // `ValidateResourceClaimName`, are for non-core-group resources
        // and are deliberately NOT wired here until their real group is
        // verified the same way).
        ("", "pods") => crate::scheme::name_format::is_dns1123_subdomain(name),
        ("", "replicationcontrollers") => crate::scheme::name_format::is_dns1123_subdomain(name),
        ("", "nodes") => crate::scheme::name_format::is_dns1123_subdomain(name),
        ("", "limitranges") => crate::scheme::name_format::is_dns1123_subdomain(name),
        ("", "resourcequotas") => crate::scheme::name_format::is_dns1123_subdomain(name),
        ("", "secrets") => crate::scheme::name_format::is_dns1123_subdomain(name),
        ("", "endpoints") => crate::scheme::name_format::is_dns1123_subdomain(name),
        ("", "persistentvolumes") => crate::scheme::name_format::is_dns1123_subdomain(name),
        ("", "configmaps") => crate::scheme::name_format::is_dns1123_subdomain(name),
        // `ValidateServiceCreate` (same file, lines ~6655-6685, read in
        // full): normally `ValidateServiceName = NameIsDNS1035Label`,
        // relaxed to `NameIsDNSLabel` only behind the
        // `RelaxedServiceNameValidation` feature gate (alpha, default
        // off). This crate has no feature-gate system, so the honest
        // default is the gate's default-off behavior: DNS1035Label.
        ("", "services") => crate::scheme::name_format::is_dns1035_label(name),
        // Non-core groups, each confirmed two ways: the real
        // `var Validate<Kind>Name = apimachineryvalidation.NameIsDNSSubdomain`
        // declaration AND the real per-type `Validate<Kind>` function that
        // actually applies it to that type's own `ObjectMeta` (not just a
        // same-named field elsewhere — `ValidateClassName`, for one, is
        // also used to check *referenced* `storageClassName` fields on
        // PV/PVC, which is a different check entirely from this one), plus
        // the group/version cross-checked against the vendored spec's own
        // `paths` table:
        // - `priorityclasses` (scheduling.k8s.io/v1):
        //   `ValidatePriorityClass` -> `NameIsDNSSubdomain` directly
        //   (inlined, not the `ValidatePriorityClassName` var — same rule).
        //   Named honestly: real upstream also forbids a `system-`-prefixed
        //   name unless it's one of a fixed predefined set
        //   (`IsKnownSystemPriorityClass`) — that check is NOT ported here,
        //   only the DNS-subdomain shape.
        // - `resourceclaims`/`resourceclaimtemplates` (resource.k8s.io/v1):
        //   `ValidateResourceClaim`/`ValidateResourceClaimTemplate` ->
        //   `ValidateResourceClaimName`/`ValidateResourceClaimTemplateName`
        //   (`pkg/apis/resource/validation/validation.go`, confirmed).
        // - `storageclasses` (storage.k8s.io/v1): `ValidateStorageClass` ->
        //   `ValidateClassName` (`pkg/apis/storage/validation/validation.go`,
        //   confirmed this is really StorageClass's own object-name check,
        //   not only the referenced-field usage).
        ("scheduling.k8s.io", "priorityclasses") => crate::scheme::name_format::is_dns1123_subdomain(name),
        ("resource.k8s.io", "resourceclaims") => crate::scheme::name_format::is_dns1123_subdomain(name),
        ("resource.k8s.io", "resourceclaimtemplates") => crate::scheme::name_format::is_dns1123_subdomain(name),
        ("storage.k8s.io", "storageclasses") => crate::scheme::name_format::is_dns1123_subdomain(name),
        // More non-core groups, same two-way verification (real
        // per-type `Validate<Kind>[Create]` function confirmed to apply
        // the var to that type's own `ObjectMeta`, real group confirmed
        // against that group's own vendored spec `paths` table):
        // `apps/v1`: ControllerRevision, DaemonSet, Deployment, ReplicaSet
        // (`pkg/apis/apps/validation/validation.go`).
        // `networking.k8s.io/v1`: Ingress, IngressClass, ServiceCIDR
        // (`pkg/apis/networking/validation/validation.go`).
        // `discovery.k8s.io/v1`: EndpointSlice
        // (`pkg/apis/discovery/validation/validation.go`).
        // `flowcontrol.apiserver.k8s.io/v1`: FlowSchema,
        // PriorityLevelConfiguration
        // (`pkg/apis/flowcontrol/validation/validation.go`).
        // `node.k8s.io/v1`: RuntimeClass — inlines `NameIsDNSSubdomain`
        // directly rather than through a named var, same rule
        // (`pkg/apis/node/validation/validation.go`).
        // `coordination.k8s.io/v1`: Lease — same inlined-not-var pattern
        // (`pkg/apis/coordination/validation/validation.go`).
        ("apps", "controllerrevisions") => crate::scheme::name_format::is_dns1123_subdomain(name),
        ("apps", "daemonsets") => crate::scheme::name_format::is_dns1123_subdomain(name),
        ("apps", "deployments") => crate::scheme::name_format::is_dns1123_subdomain(name),
        ("apps", "replicasets") => crate::scheme::name_format::is_dns1123_subdomain(name),
        ("networking.k8s.io", "ingresses") => crate::scheme::name_format::is_dns1123_subdomain(name),
        ("networking.k8s.io", "ingressclasses") => crate::scheme::name_format::is_dns1123_subdomain(name),
        ("networking.k8s.io", "servicecidrs") => crate::scheme::name_format::is_dns1123_subdomain(name),
        ("discovery.k8s.io", "endpointslices") => crate::scheme::name_format::is_dns1123_subdomain(name),
        ("flowcontrol.apiserver.k8s.io", "flowschemas") => crate::scheme::name_format::is_dns1123_subdomain(name),
        ("flowcontrol.apiserver.k8s.io", "prioritylevelconfigurations") => crate::scheme::name_format::is_dns1123_subdomain(name),
        ("node.k8s.io", "runtimeclasses") => crate::scheme::name_format::is_dns1123_subdomain(name),
        ("coordination.k8s.io", "leases") => crate::scheme::name_format::is_dns1123_subdomain(name),
        _ => Vec::new(),
    }
}

/// No-ops (rather than panicking, matching this crate's established
/// "malformed/adversarial input degrades gracefully" posture) if `object`
/// isn't itself a JSON object — `serde_json::Value`'s `IndexMut` panics
/// on a non-object, non-null receiver, and a request body that made it
/// this far without being an object at all is a real, if unlikely, case
/// (an empty-`required`-list schema lets `validate_required`/
/// `validate_types` both pass on one).
fn set_metadata_field(object: &mut Value, field: &str, value: Value) {
    let Some(map) = object.as_object_mut() else { return };
    let metadata = map.entry("metadata").or_insert_with(|| json!({}));
    if !metadata.is_object() {
        *metadata = json!({});
    }
    metadata[field] = value;
}

/// Allocates the short suffix used by the API server for generateName.
fn generate_name(prefix: &str) -> String {
    let suffix = uuid::Uuid::new_v4().simple().to_string();
    format!("{prefix}{}", &suffix[..5])
}

fn set_type_metadata(object: &mut Value, kind: &str, api_version: &str) {
    let Some(map) = object.as_object_mut() else { return };
    map.insert("kind".to_string(), Value::String(kind.to_string()));
    map.insert("apiVersion".to_string(), Value::String(api_version.to_string()));
}

/// Second-precision RFC3339 with a `Z` suffix (`"2026-08-20T09:30:00Z"`)
/// — matches real upstream's own `metav1.Time` marshaling, which never
/// carries sub-second precision.
fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

#[derive(Debug, PartialEq)]
pub enum DeleteOutcome {
    /// The object as it was immediately before deletion — real upstream's
    /// own synchronous-delete response shape (not a bare `Status`, unless
    /// the caller specifically asked for one, which this build doesn't
    /// yet distinguish).
    Deleted(Value),
    UnknownResource,
    ObjectNotFound,
    /// The requested `resourceVersion` or `uid` did not match the live
    /// object. Kubernetes reports this as a conflict and leaves it intact.
    PreconditionFailed,
}

/// Deletes a single object. `namespace: None` for a cluster-scoped
/// resource, same convention as [`get`]/[`list`]/[`create`].
pub async fn delete(storage: &mut StorageClient, group: &str, version: &str, resource: &str, namespace: Option<&str>, name: &str) -> Result<DeleteOutcome, Error> {
    delete_with_options(storage, group, version, resource, namespace, name, None, false).await
}

/// The subset of Kubernetes `DeleteOptions.preconditions` that can be
/// enforced against nodestore's MVCC-backed objects.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DeletePreconditions {
    pub resource_version: Option<String>,
    pub uid: Option<String>,
}

/// Deletes a single object with optional `DeleteOptions` preconditions and
/// `dryRun=All`. The read and delete are joined by an MVCC compare so a
/// concurrent update cannot make a validated delete remove a newer object.
pub async fn delete_with_options(
    storage: &mut StorageClient,
    group: &str,
    version: &str,
    resource: &str,
    namespace: Option<&str>,
    name: &str,
    preconditions: Option<&DeletePreconditions>,
    dry_run: bool,
) -> Result<DeleteOutcome, Error> {
    if resolve_resource(storage, group, version, resource).await?.is_none() {
        return Ok(DeleteOutcome::UnknownResource);
    }
    let key = keys::object_key(group, resource, namespace, name);
    let current = storage.range(RangeRequest { key: key.clone().into_bytes(), ..Default::default() }).await?;
    let Some(prev) = current.kvs.into_iter().next() else {
        return Ok(DeleteOutcome::ObjectNotFound);
    };
    let mut object = decrypt_and_decode(storage, group, resource, &prev.key, &prev.value)?;
    set_metadata_field(&mut object, "resourceVersion", Value::String(prev.mod_revision.to_string()));
    if let Some(preconditions) = preconditions {
        if let Some(resource_version) = &preconditions.resource_version {
            let matches = resource_version.parse::<i64>().ok() == Some(prev.mod_revision);
            if !matches {
                return Ok(DeleteOutcome::PreconditionFailed);
            }
        }
        if let Some(uid) = &preconditions.uid {
            if object.pointer("/metadata/uid").and_then(Value::as_str) != Some(uid.as_str()) {
                return Ok(DeleteOutcome::PreconditionFailed);
            }
        }
    }
    let kind = object["kind"].as_str().unwrap_or("Unknown").to_string();
    let object = crate::scheme::conversion::to_version(group, version, &kind, object);
    if dry_run {
        return Ok(DeleteOutcome::Deleted(object));
    }

    let compare = pb::Compare {
        key: key.clone().into_bytes(),
        result: pb::compare::CompareResult::Equal as i32,
        target: pb::compare::CompareTarget::Mod as i32,
        target_union: Some(pb::compare::TargetUnion::ModRevision(prev.mod_revision)),
        range_end: Vec::new(),
    };
    let txn = pb::TxnRequest {
        compare: vec![compare],
        success: vec![pb::RequestOp {
            request: Some(pb::request_op::Request::RequestDeleteRange(pb::DeleteRangeRequest {
                key: key.into_bytes(),
                prev_kv: true,
                ..Default::default()
            })),
        }],
        failure: vec![],
    };
    let response = storage.txn(txn).await?;
    if !response.succeeded {
        return Ok(DeleteOutcome::PreconditionFailed);
    }
    Ok(DeleteOutcome::Deleted(object))
}

#[derive(Debug, PartialEq)]
pub enum DeleteCollectionOutcome {
    /// The `<Kind>List` of every object that matched, exactly as it
    /// listed immediately before any of them were deleted — real
    /// upstream's own `Store.DeleteCollection` response shape (it
    /// returns the `List` object it read at the start, not one rebuilt
    /// after the fact).
    Deleted(Value),
    UnknownResource,
}

/// Real upstream's own `Store.DeleteCollection`
/// (`k8s.io/apiserver/pkg/registry/generic/registry/store.go`, fetched
/// and read directly), scoped down: lists every object matching
/// `label_selector`/`field_selector` (reusing [`list`]'s own selector
/// parsing — the exact same filtering a real `DELETE .../pods` collection
/// request would apply), then deletes each one by name via [`delete`],
/// silently ignoring one that's already gone (`ObjectNotFound` — matches
/// real upstream's own `!apierrors.IsNotFound(err)` guard: a concurrent
/// delete of the same object isn't a collection-delete failure). Returns
/// the pre-deletion `List`, the same real response shape a single
/// `DELETE`'s own "the object as it was immediately before deletion"
/// convention already established for one object at a time.
/// **Named, honest simplification**: real upstream deletes with a
/// worker pool (`DeleteCollectionWorkers`, concurrent); this port
/// deletes sequentially. It also always lists everything in one
/// unpaginated shot (`limit: 0`) regardless of how large the collection
/// is — real upstream's own collection delete paginates its internal
/// listing too, which this doesn't. A per-item deletion error *other
/// than* not-found still aborts the whole call and surfaces as a real
/// `500` — real upstream's own posture too (`errs <- err` stops the
/// collection short).
pub async fn delete_collection(storage: &mut StorageClient, group: &str, version: &str, resource: &str, namespace: Option<&str>, label_selector: &str, field_selector: &str) -> Result<DeleteCollectionOutcome, Error> {
    let listed = list(storage, None, group, version, resource, namespace, label_selector, field_selector, 0, "").await?;
    let ListOutcome::Found(list_value) = listed else {
        return Ok(DeleteCollectionOutcome::UnknownResource);
    };
    let items = list_value["items"].as_array().cloned().unwrap_or_default();
    for item in &items {
        let Some(name) = item.pointer("/metadata/name").and_then(Value::as_str) else { continue };
        delete(storage, group, version, resource, namespace, name).await?;
    }
    Ok(DeleteCollectionOutcome::Deleted(list_value))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn continue_token_round_trips_the_resume_key_and_revision() {
        let token = encode_continue_token(b"/registry/pods/default/my-pod\x00", 42);
        let (key, revision) = decode_continue_token(&token).expect("a token this module encoded must decode");
        assert_eq!(key, b"/registry/pods/default/my-pod\x00");
        assert_eq!(revision, 42);
    }

    #[test]
    fn continue_token_rejects_invalid_base64() {
        assert!(decode_continue_token("not valid base64!!!").is_none());
    }

    #[test]
    fn continue_token_rejects_a_missing_separator() {
        use base64::Engine;
        let no_separator = base64::engine::general_purpose::STANDARD.encode(b"no-null-byte-here");
        assert!(decode_continue_token(&no_separator).is_none());
    }

    #[test]
    fn continue_token_rejects_a_non_numeric_revision() {
        use base64::Engine;
        let mut buf = b"/registry/pods/default/x".to_vec();
        buf.push(0);
        buf.extend_from_slice(b"not-a-number");
        let bad = base64::engine::general_purpose::STANDARD.encode(buf);
        assert!(decode_continue_token(&bad).is_none());
    }

    #[test]
    fn resolve_kind_finds_a_real_known_resource() {
        assert_eq!(resolve_kind("", "v1", "pods"), Some("Pod"));
        assert_eq!(resolve_kind("apps", "v1", "deployments"), Some("Deployment"));
    }

    #[test]
    fn resolve_kind_finds_namespaced_rbac_resources() {
        for (resource, kind, schema) in [
            ("roles", "Role", "io.k8s.api.rbac.v1.Role"),
            ("rolebindings", "RoleBinding", "io.k8s.api.rbac.v1.RoleBinding"),
        ] {
            assert_eq!(resolve_kind("rbac.authorization.k8s.io", "v1", resource), Some(kind));
            assert_eq!(
                protobuf::schema_for_gvk("rbac.authorization.k8s.io", "v1", kind),
                Some(schema)
            );
        }
    }

    #[test]
    fn resolve_kind_is_none_for_an_unknown_resource_or_group_version() {
        assert_eq!(resolve_kind("", "v1", "totally-made-up"), None);
        assert_eq!(resolve_kind("totally.made.up", "v1", "pods"), None);
    }

    #[test]
    fn split_api_version_handles_core_and_grouped_forms() {
        assert_eq!(split_api_version("v1"), ("", "v1"));
        assert_eq!(split_api_version("apps/v1"), ("apps", "v1"));
    }

    /// The real round trip: encode a Namespace object the same way a
    /// write path would (`encode_message` + `wrap_unknown`), then prove
    /// `decode_stored_object` gets the exact same JSON back out.
    #[test]
    fn decode_stored_object_round_trips_a_real_encoded_object() {
        let schema = protobuf::schema_for_gvk("", "v1", "Namespace").expect("core/v1 Namespace should be a known schema");
        let value = json!({"metadata": {"name": "default"}});
        let object_bytes = protobuf::encode_message(schema, &value).unwrap();
        let envelope = protobuf::wrap_unknown("v1", "Namespace", &object_bytes);

        let decoded = decode_stored_object(&envelope).unwrap();
        assert_eq!(decoded["metadata"]["name"], "default");
    }

    #[test]
    fn decode_stored_object_rejects_a_non_envelope_payload() {
        assert!(decode_stored_object(b"not an envelope at all").is_err());
    }

    #[test]
    fn list_kind_appends_list_to_the_real_kind() {
        assert_eq!(list_kind("Pod"), "PodList");
        assert_eq!(list_kind("Deployment"), "DeploymentList");
    }

    #[test]
    fn name_format_violations_enforces_the_real_namespace_rule() {
        assert!(name_format_violations("", "namespaces", "my-namespace").is_empty());
        assert!(!name_format_violations("", "namespaces", "My_Namespace").is_empty());
    }

    #[test]
    fn name_format_violations_enforces_the_real_serviceaccount_rule() {
        assert!(name_format_violations("", "serviceaccounts", "my.sa-name").is_empty());
        assert!(!name_format_violations("", "serviceaccounts", "My_SA").is_empty());
    }

    #[test]
    fn patch_kind_for_content_type_recognizes_all_three_real_media_types() {
        assert_eq!(patch_kind_for_content_type("application/json-patch+json"), Some(PatchKind::Json));
        assert_eq!(patch_kind_for_content_type("application/merge-patch+json"), Some(PatchKind::Merge));
        assert_eq!(patch_kind_for_content_type("application/strategic-merge-patch+json"), Some(PatchKind::StrategicMerge));
    }

    #[test]
    fn patch_kind_for_content_type_ignores_charset_parameters() {
        assert_eq!(patch_kind_for_content_type("application/merge-patch+json; charset=utf-8"), Some(PatchKind::Merge));
    }

    #[test]
    fn patch_kind_for_content_type_rejects_unknown_or_ssa_media_types() {
        assert_eq!(patch_kind_for_content_type("application/json"), None);
        // Server-Side Apply's own media type isn't recognized -- Group G's
        // own doc comment already names SSA/managedFields as not landed.
        assert_eq!(patch_kind_for_content_type("application/apply-patch+yaml"), None);
        assert_eq!(patch_kind_for_content_type(""), None);
    }

    #[test]
    fn omitted_content_type_uses_strategic_merge_for_builtins_and_merge_for_crds() {
        assert_eq!(default_patch_kind(false), PatchKind::StrategicMerge);
        assert_eq!(default_patch_kind(true), PatchKind::Merge);
    }

    #[test]
    fn name_format_violations_is_empty_for_a_resource_with_no_verified_rule() {
        // events has no verified per-type name rule wired in yet -- must
        // not invent a check for it.
        assert!(name_format_violations("", "events", "Not-A-Valid-DNS-Label-But-Unchecked").is_empty());
    }

    #[test]
    fn name_format_violations_enforces_the_real_dns_subdomain_rule_on_each_verified_resource() {
        for resource in ["pods", "replicationcontrollers", "nodes", "limitranges", "resourcequotas", "secrets", "endpoints", "persistentvolumes", "configmaps"] {
            assert!(name_format_violations("", resource, "my-name.example").is_empty(), "{resource} should accept a valid DNS subdomain");
            assert!(!name_format_violations("", resource, "My_Bad_Name").is_empty(), "{resource} should reject an invalid DNS subdomain");
        }
    }

    #[test]
    fn name_format_violations_enforces_the_real_dns_subdomain_rule_on_each_verified_non_core_resource() {
        for (group, resource) in [
            ("scheduling.k8s.io", "priorityclasses"),
            ("resource.k8s.io", "resourceclaims"),
            ("resource.k8s.io", "resourceclaimtemplates"),
            ("storage.k8s.io", "storageclasses"),
        ] {
            assert!(name_format_violations(group, resource, "my-name.example").is_empty(), "{group}/{resource} should accept a valid DNS subdomain");
            assert!(!name_format_violations(group, resource, "My_Bad_Name").is_empty(), "{group}/{resource} should reject an invalid DNS subdomain");
        }
        // The same resource name under the wrong group must not match --
        // this table is keyed on (group, resource), not resource alone.
        assert!(name_format_violations("", "priorityclasses", "My_Bad_Name").is_empty());
    }

    #[test]
    fn name_format_violations_enforces_the_real_dns_subdomain_rule_on_each_newly_verified_resource() {
        for (group, resource) in [
            ("apps", "controllerrevisions"),
            ("apps", "daemonsets"),
            ("apps", "deployments"),
            ("apps", "replicasets"),
            ("networking.k8s.io", "ingresses"),
            ("networking.k8s.io", "ingressclasses"),
            ("networking.k8s.io", "servicecidrs"),
            ("discovery.k8s.io", "endpointslices"),
            ("flowcontrol.apiserver.k8s.io", "flowschemas"),
            ("flowcontrol.apiserver.k8s.io", "prioritylevelconfigurations"),
            ("node.k8s.io", "runtimeclasses"),
            ("coordination.k8s.io", "leases"),
        ] {
            assert!(name_format_violations(group, resource, "my-name.example").is_empty(), "{group}/{resource} should accept a valid DNS subdomain");
            assert!(!name_format_violations(group, resource, "My_Bad_Name").is_empty(), "{group}/{resource} should reject an invalid DNS subdomain");
        }
    }

    #[test]
    fn name_format_violations_enforces_the_real_service_dns1035_rule() {
        // DNS1035Label: must start with a letter, no leading digit and no
        // '.' (both allowed in a DNS1123 subdomain) -- proves this isn't
        // silently sharing the subdomain check.
        assert!(name_format_violations("", "services", "my-svc").is_empty());
        assert!(!name_format_violations("", "services", "1-starts-with-digit").is_empty());
        assert!(!name_format_violations("", "services", "has.a.dot").is_empty());
    }

    #[test]
    fn now_rfc3339_has_no_subsecond_precision_and_a_z_suffix() {
        let ts = now_rfc3339();
        assert!(ts.ends_with('Z'), "got {ts:?}");
        assert!(!ts.contains('.'), "must be second-precision only, got {ts:?}");
        // A real, parseable RFC3339 timestamp round-trips through chrono.
        assert!(chrono::DateTime::parse_from_rfc3339(&ts).is_ok(), "not valid RFC3339: {ts:?}");
    }

    #[test]
    fn set_metadata_field_creates_metadata_when_absent() {
        let mut obj = json!({});
        set_metadata_field(&mut obj, "uid", Value::String("abc".to_string()));
        assert_eq!(obj["metadata"]["uid"], "abc");
    }

    #[test]
    fn set_metadata_field_preserves_existing_metadata_fields() {
        let mut obj = json!({"metadata": {"name": "web-1"}});
        set_metadata_field(&mut obj, "uid", Value::String("abc".to_string()));
        assert_eq!(obj["metadata"]["name"], "web-1");
        assert_eq!(obj["metadata"]["uid"], "abc");
    }

    #[test]
    fn generated_name_appends_a_unique_five_character_suffix() {
        let first = generate_name("job-");
        let second = generate_name("job-");
        assert!(first.starts_with("job-"));
        assert_eq!(first.len(), "job-".len() + 5);
        assert_ne!(first, second);
    }

    #[test]
    fn protobuf_request_decodes_a_built_in_object_envelope() {
        let schema = protobuf::schema_for_gvk("", "v1", "ConfigMap").expect("ConfigMap has a generated schema");
        let encoded = protobuf::encode_message(schema, &json!({
            "metadata": {"name": "from-protobuf"},
            "data": {"key": "value"}
        })).unwrap();
        let envelope = protobuf::wrap_unknown("v1", "ConfigMap", &encoded);
        let resolved = ResolvedResource {
            kind: "ConfigMap".to_string(),
            schema: Some(schema),
            open_api_schema: None,
            has_status_subresource: true,
        };
        let decoded = decode_protobuf_object(&resolved, "configmaps", &envelope).unwrap();
        assert_eq!(decoded["apiVersion"], "v1");
        assert_eq!(decoded["kind"], "ConfigMap");
        assert_eq!(decoded["metadata"]["name"], "from-protobuf");
        assert_eq!(decoded["data"]["key"], "value");
    }

    #[test]
    fn protobuf_request_rejects_a_kind_that_does_not_match_the_resource() {
        let resolved = ResolvedResource {
            kind: "ConfigMap".to_string(),
            schema: None,
            open_api_schema: None,
            has_status_subresource: true,
        };
        let envelope = protobuf::wrap_unknown("v1", "Secret", br#"{}"#);
        assert!(matches!(decode_protobuf_object(&resolved, "configmaps", &envelope), Err(Error::InvalidProtobufRequest(_))));
    }
}
