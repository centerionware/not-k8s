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
//! miss is). `server::listener` actually does this for a deliberately
//! bounded list of resources (`server::listener`'s own
//! `BOOT_CACHED_RESOURCES`); every resource outside that list still
//! passes `None` to both. `create`/`update`/`delete` still read/write
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
//! opaque resume-key encoding — see `list`'s own doc comment). `list`'s
//! remaining real gap: no `resourceVersion`-pinned reads (always reads
//! at the current revision).
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
//! a time. `update` runs the exact same two checks. Named honestly, not
//! overclaimed: no `generateName` (a request with no `metadata.name` is
//! rejected, not given a generated one), no dry-run, no field-manager/
//! Server-Side Apply bookkeeping, no admission plugins at all (Group J
//! doesn't exist yet — a real cluster's `ServiceAccount`/
//! `NamespaceLifecycle`/`ResourceQuota`/... plugins would each get a say
//! here and don't).
//!
//! `delete` is a single `DeleteRange` (`prev_kv: true` so the deleted
//! object can be returned, matching real upstream's own synchronous
//! delete response) with no preconditions yet: no
//! `resourceVersion`/`uid` precondition checking
//! (`metav1.DeleteOptions.Preconditions`), no `propagationPolicy`
//! (Foreground/Background/Orphan — this build has no owner-reference
//! garbage collector to orphan or cascade to in the first place), no
//! finalizer handling at all. A real, unconditional delete-if-present,
//! named honestly as the bring-up floor rather than the real thing.
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

use crate::cacher::selector::{self, ParseError};
use crate::codec::protobuf;
use crate::codegen;
use crate::scheme::{defaulting, validation};
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
    let schema = protobuf::schema_for_gvk(group, version, &kind).ok_or_else(|| protobuf::Error::UnknownMessage(format!("{api_version}/{kind}")))?;
    protobuf::decode_message(schema, &object_bytes)
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
/// `None` behaves exactly as before this parameter existed — the only
/// real call site passing `Some` today is `server::listener`'s own
/// `BOOT_CACHED_RESOURCES` list (see that module's own doc comment);
/// every resource outside that list, and every other caller, still
/// passes `None`.
pub async fn get(storage: &mut StorageClient, cache: Option<&crate::cacher::store::SharedCache>, group: &str, version: &str, resource: &str, namespace: Option<&str>, name: &str) -> Result<GetOutcome, Error> {
    if resolve_kind(group, version, resource).is_none() {
        return Ok(GetOutcome::UnknownResource);
    }
    let key = keys::object_key(group, resource, namespace, name);

    if let Some(cache) = cache {
        if let Some(entry) = cache.get(key.as_bytes()) {
            let object = decode_stored_object(&entry.value)?;
            return Ok(GetOutcome::Found(object));
        }
    }

    let resp = storage.range(RangeRequest { key: key.into_bytes(), ..Default::default() }).await?;
    let Some(kv) = resp.kvs.into_iter().next() else {
        return Ok(GetOutcome::ObjectNotFound);
    };
    let object = decode_stored_object(&kv.value)?;
    Ok(GetOutcome::Found(object))
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
/// would. `None` behaves exactly as before this parameter existed; every
/// call site but `server::listener`'s own `BOOT_CACHED_RESOURCES` list
/// still passes `None` (same scope `get`'s own cache parameter is at).
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
    let Some(kind) = resolve_kind(group, version, resource) else {
        return Ok(ListOutcome::UnknownResource);
    };
    let label_reqs = if label_selector.is_empty() { Vec::new() } else { selector::parse_label_selector(label_selector)? };
    let field_reqs = if field_selector.is_empty() { Vec::new() } else { selector::parse_field_selector(field_selector)? };

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
    let paginated = limit > 0 || !continue_token.is_empty();
    if let Some(cache) = cache {
        if cache.has_synced() && !paginated {
            let (entries, revision) = cache.list();
            let items = entries
                .iter()
                .filter(|(key, _)| key.starts_with(&prefix))
                .map(|(_, entry)| decode_stored_object(&entry.value))
                .collect::<Result<Vec<Value>, protobuf::Error>>()?
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
        (prefix, 0)
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
        .map(|kv| decode_stored_object(&kv.value))
        .collect::<Result<Vec<Value>, protobuf::Error>>()?
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
    /// No `metadata.name` in the submitted body — this build doesn't
    /// support `generateName`, named honestly rather than silently
    /// treating it as a name (see this module's own doc comment).
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
    let Some(kind) = resolve_kind(group, version, resource) else {
        return Ok(CreateOutcome::UnknownResource);
    };
    let Some(schema) = protobuf::schema_for_gvk(group, version, kind) else {
        return Ok(CreateOutcome::UnknownResource);
    };

    let Some(name) = body.pointer("/metadata/name").and_then(Value::as_str).filter(|n| !n.is_empty()) else {
        return Ok(CreateOutcome::MissingName);
    };
    let name = name.to_string();

    if let (Some(ns), Some(body_ns)) = (namespace, body.pointer("/metadata/namespace").and_then(Value::as_str)) {
        if !body_ns.is_empty() && body_ns != ns {
            return Ok(CreateOutcome::NamespaceMismatch);
        }
    }

    let mut violations: Vec<String> = validation::validate_required(schema, body).into_iter().map(|m| format!("{}: Required value", m.path)).collect();
    violations.extend(validation::validate_types(schema, body).into_iter().map(|t| format!("{}: expected type {}, got {}", t.path, t.expected, t.actual_kind)));
    violations.extend(name_format_violations(group, resource, &name).into_iter().map(|e| format!("metadata.name: {e}")));
    if !violations.is_empty() {
        return Ok(CreateOutcome::Invalid(violations));
    }

    let mut object = defaulting::apply_defaults(schema, body);
    set_metadata_field(&mut object, "creationTimestamp", Value::String(now_rfc3339()));
    set_metadata_field(&mut object, "uid", Value::String(uuid::Uuid::new_v4().to_string()));
    if let Some(ns) = namespace {
        set_metadata_field(&mut object, "namespace", Value::String(ns.to_string()));
    }

    let key = keys::object_key(group, resource, namespace, &name);
    let api_version = if group.is_empty() { version.to_string() } else { format!("{group}/{version}") };
    let object_bytes = protobuf::encode_message(schema, &object)?;
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
    let Some(kind) = resolve_kind(group, version, resource) else {
        return Ok(UpdateOutcome::UnknownResource);
    };
    let Some(schema) = protobuf::schema_for_gvk(group, version, kind) else {
        return Ok(UpdateOutcome::UnknownResource);
    };

    let key = keys::object_key(group, resource, namespace, name);
    let existing_resp = storage.range(RangeRequest { key: key.clone().into_bytes(), ..Default::default() }).await?;
    let Some(existing_kv) = existing_resp.kvs.into_iter().next() else {
        return Ok(UpdateOutcome::ObjectNotFound);
    };
    let existing_object = decode_stored_object(&existing_kv.value)?;

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

    let mut violations: Vec<String> = validation::validate_required(schema, body).into_iter().map(|m| format!("{}: Required value", m.path)).collect();
    violations.extend(validation::validate_types(schema, body).into_iter().map(|t| format!("{}: expected type {}, got {}", t.path, t.expected, t.actual_kind)));
    violations.extend(name_format_violations(group, resource, name).into_iter().map(|e| format!("metadata.name: {e}")));
    if !violations.is_empty() {
        return Ok(UpdateOutcome::Invalid(violations));
    }

    let object = defaulting::apply_defaults(schema, body);
    persist_update(storage, schema, kind, group, version, key, &existing_kv, &existing_object, namespace, object).await
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
pub async fn update_status(storage: &mut StorageClient, group: &str, version: &str, resource: &str, namespace: Option<&str>, name: &str, body: &Value) -> Result<UpdateOutcome, Error> {
    let Some(kind) = resolve_kind(group, version, resource) else {
        return Ok(UpdateOutcome::UnknownResource);
    };
    let Some(schema) = protobuf::schema_for_gvk(group, version, kind) else {
        return Ok(UpdateOutcome::UnknownResource);
    };

    let key = keys::object_key(group, resource, namespace, name);
    let existing_resp = storage.range(RangeRequest { key: key.clone().into_bytes(), ..Default::default() }).await?;
    let Some(existing_kv) = existing_resp.kvs.into_iter().next() else {
        return Ok(UpdateOutcome::ObjectNotFound);
    };
    let existing_object = decode_stored_object(&existing_kv.value)?;

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

    persist_update(storage, schema, kind, group, version, key, &existing_kv, &existing_object, namespace, object).await
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
    schema: &str,
    kind: &str,
    group: &str,
    version: &str,
    key: String,
    existing_kv: &mvccpb::KeyValue,
    existing_object: &Value,
    namespace: Option<&str>,
    mut object: Value,
) -> Result<UpdateOutcome, Error> {
    for field in ["creationTimestamp", "uid"] {
        if let Some(existing_value) = existing_object.pointer(&format!("/metadata/{field}")).cloned() {
            set_metadata_field(&mut object, field, existing_value);
        }
    }
    if let Some(ns) = namespace {
        set_metadata_field(&mut object, "namespace", Value::String(ns.to_string()));
    }

    let api_version = if group.is_empty() { version.to_string() } else { format!("{group}/{version}") };
    let object_bytes = protobuf::encode_message(schema, &object)?;
    let envelope = protobuf::wrap_unknown(&api_version, kind, &object_bytes);

    let compare = pb::Compare {
        key: key.clone().into_bytes(),
        result: pb::compare::CompareResult::Equal as i32,
        target: pb::compare::CompareTarget::Mod as i32,
        target_union: Some(pb::compare::TargetUnion::ModRevision(existing_kv.mod_revision)),
        range_end: Vec::new(),
    };
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
/// `patch::*` module applies each (`content_type_matches_patch_kind`'s
/// own doc comment covers the one real narrowing: this build has no
/// per-resource default patch-strategy concept, so a caller must send an
/// explicit `Content-Type`, unlike real upstream's own
/// `kubectl patch`, which defaults to strategic-merge for built-in types
/// and falls back to merge-patch for CRDs when none is given).
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

/// The context [`patch_prepare`] hands back to [`patch_persist`] once a
/// patch has been applied but before it's validated/persisted — enough
/// to run Group J admission against the real candidate object in
/// between (`server::listener`'s own `PATCH` branch does exactly this
/// for `LimitRanger`), without re-fetching or re-applying the patch a
/// second time.
pub struct PatchContext {
    schema: &'static str,
    kind: &'static str,
    key: String,
    existing_kv: mvccpb::KeyValue,
    existing_object: Value,
}

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
fn apply_patch(kind_of_patch: PatchKind, schema: &'static str, existing: &Value, patch_doc: &Value) -> Result<Value, String> {
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
        PatchKind::StrategicMerge => Ok(crate::patch::strategic_merge::apply(schema, existing, patch_doc)),
    }
}

/// Reads the current object and applies one of this build's three real
/// patch kinds to it — the "prepare" half of [`patch`], split out so a
/// caller (`server::listener`) can run Group J admission against the
/// real candidate object before committing to [`patch_persist`].
pub async fn patch_prepare(storage: &mut StorageClient, group: &str, version: &str, resource: &str, namespace: Option<&str>, name: &str, kind_of_patch: PatchKind, patch_doc: &Value) -> Result<PatchPrepareOutcome, Error> {
    let Some(kind) = resolve_kind(group, version, resource) else {
        return Ok(PatchPrepareOutcome::UnknownResource);
    };
    let Some(schema) = protobuf::schema_for_gvk(group, version, kind) else {
        return Ok(PatchPrepareOutcome::UnknownResource);
    };

    let key = keys::object_key(group, resource, namespace, name);
    let existing_resp = storage.range(RangeRequest { key: key.clone().into_bytes(), ..Default::default() }).await?;
    let Some(existing_kv) = existing_resp.kvs.into_iter().next() else {
        return Ok(PatchPrepareOutcome::ObjectNotFound);
    };
    let existing_object = decode_stored_object(&existing_kv.value)?;

    let patched = match apply_patch(kind_of_patch, schema, &existing_object, patch_doc) {
        Ok(object) => object,
        Err(msg) => return Ok(PatchPrepareOutcome::Invalid(vec![msg])),
    };

    Ok(PatchPrepareOutcome::Ready(patched, PatchContext { schema, kind, key, existing_kv, existing_object }))
}

/// The "persist" half of [`patch`]: validates/defaults `candidate` (the
/// object [`patch_prepare`] produced, possibly further mutated by
/// admission in between) and writes it with the same real optimistic
/// concurrency [`update`] uses (`Txn`-compared-against-`ModRevision`,
/// via the shared [`persist_update`] tail) — no client-submitted
/// `resourceVersion` needed, since the object being patched *is* the one
/// [`patch_prepare`] already read.
pub async fn patch_persist(storage: &mut StorageClient, group: &str, version: &str, resource: &str, namespace: Option<&str>, name: &str, context: PatchContext, candidate: Value) -> Result<UpdateOutcome, Error> {
    let mut violations: Vec<String> = validation::validate_required(context.schema, &candidate).into_iter().map(|m| format!("{}: Required value", m.path)).collect();
    violations.extend(validation::validate_types(context.schema, &candidate).into_iter().map(|t| format!("{}: expected type {}, got {}", t.path, t.expected, t.actual_kind)));
    violations.extend(name_format_violations(group, resource, name).into_iter().map(|e| format!("metadata.name: {e}")));
    if !violations.is_empty() {
        return Ok(UpdateOutcome::Invalid(violations));
    }

    let object = defaulting::apply_defaults(context.schema, &candidate);
    persist_update(storage, context.schema, context.kind, group, version, context.key, &context.existing_kv, &context.existing_object, namespace, object).await
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
/// validation, no Group J admission.
pub async fn patch_status(storage: &mut StorageClient, group: &str, version: &str, resource: &str, namespace: Option<&str>, name: &str, kind_of_patch: PatchKind, patch_doc: &Value) -> Result<UpdateOutcome, Error> {
    let Some(kind) = resolve_kind(group, version, resource) else {
        return Ok(UpdateOutcome::UnknownResource);
    };
    let Some(schema) = protobuf::schema_for_gvk(group, version, kind) else {
        return Ok(UpdateOutcome::UnknownResource);
    };

    let key = keys::object_key(group, resource, namespace, name);
    let existing_resp = storage.range(RangeRequest { key: key.clone().into_bytes(), ..Default::default() }).await?;
    let Some(existing_kv) = existing_resp.kvs.into_iter().next() else {
        return Ok(UpdateOutcome::ObjectNotFound);
    };
    let existing_object = decode_stored_object(&existing_kv.value)?;

    let patched = match apply_patch(kind_of_patch, schema, &existing_object, patch_doc) {
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

    persist_update(storage, schema, kind, group, version, key, &existing_kv, &existing_object, namespace, object).await
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
}

/// Deletes a single object. `namespace: None` for a cluster-scoped
/// resource, same convention as [`get`]/[`list`]/[`create`].
pub async fn delete(storage: &mut StorageClient, group: &str, version: &str, resource: &str, namespace: Option<&str>, name: &str) -> Result<DeleteOutcome, Error> {
    if resolve_kind(group, version, resource).is_none() {
        return Ok(DeleteOutcome::UnknownResource);
    }
    let key = keys::object_key(group, resource, namespace, name);
    let resp = storage.delete_range(pb::DeleteRangeRequest { key: key.into_bytes(), prev_kv: true, ..Default::default() }).await?;
    let Some(prev) = resp.prev_kvs.into_iter().next() else {
        return Ok(DeleteOutcome::ObjectNotFound);
    };
    let object = decode_stored_object(&prev.value)?;
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
}
