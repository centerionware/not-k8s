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
//! (`POST /api/v1/namespaces/{ns}/pods`), and now single-object `DELETE`
//! (`DELETE /api/v1/namespaces/{ns}/pods/{name}`) — `watch`/`update`/
//! `patch`/`deletecollection` all remain the bring-up echo stub
//! (`server::listener`'s own doc comment). Reads go straight to
//! `storage::client::StorageClient::range`, bypassing
//! `cacher::store::WatchCache` entirely — a real, valid read strategy
//! (upstream's own quorum-read / watch-cache-disabled path takes exactly
//! this shape), not a shortcut standing in for the cache; wiring the
//! cache in is separate follow-up work, blocked on `cacher::driver`'s own
//! reconnect loop actually being started at boot (today nothing in
//! `lib.rs::run()` calls it). No authentication is consulted *inside*
//! this module either way — `server::listener` is what applies Group
//! H/I's identity/RBAC (opt-in, see that module's own doc comment)
//! before ever calling in here; no admission (Group J) exists at all yet.
//! Subresources (`pods/status`, `pods/log`, ...) aren't handled — the
//! discovery table this module reads doesn't carry them either (a named,
//! separate skip in `build/discovery_parse.rs`). `list` filters by
//! label/field selector for real (`cacher::selector::object_matches`,
//! wired against every item's own decoded JSON — Group D's own generic
//! adapter, unchanged here). `list`'s remaining real gaps: no pagination
//! (`continue`/`limit`), no `resourceVersion`-pinned reads (always reads
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
//! Named honestly, not overclaimed: no `generateName` (a request with no
//! `metadata.name` is rejected, not given a generated one), no dry-run,
//! no field-manager/Server-Side Apply bookkeeping, no admission plugins
//! at all (Group J doesn't exist yet — a real cluster's `ServiceAccount`/
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

use crate::cacher::selector::{self, ParseError};
use crate::codec::protobuf;
use crate::codegen;
use crate::scheme::{defaulting, validation};
use crate::storage::client::{prefix_range_end, Error as StorageError, StorageClient};
use crate::storage::keys;
use crate::storage::pb::etcdserverpb as pb;
use crate::storage::pb::etcdserverpb::RangeRequest;
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
pub async fn get(storage: &mut StorageClient, group: &str, version: &str, resource: &str, namespace: Option<&str>, name: &str) -> Result<GetOutcome, Error> {
    if resolve_kind(group, version, resource).is_none() {
        return Ok(GetOutcome::UnknownResource);
    }
    let key = keys::object_key(group, resource, namespace, name);
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
/// convention as [`get`]). One real `Range` request over the resource's
/// key prefix (`storage::keys::list_prefix` + `prefix_range_end`),
/// decoding every returned value the same way `get` does. Items are
/// returned in whatever order nodestore's own `Range` returns them in
/// (key order) — real upstream doesn't guarantee list ordering either.
/// `label_selector`/`field_selector` are the raw query-string values
/// `path::RequestInfo` already captures for `list` (empty means "no
/// constraint from that half," matching upstream's own `Everything()`
/// selector semantics — see `cacher::selector::object_matches`, the
/// predicate this filters with once both are parsed).
pub async fn list(storage: &mut StorageClient, group: &str, version: &str, resource: &str, namespace: Option<&str>, label_selector: &str, field_selector: &str) -> Result<ListOutcome, Error> {
    let Some(kind) = resolve_kind(group, version, resource) else {
        return Ok(ListOutcome::UnknownResource);
    };
    let label_reqs = if label_selector.is_empty() { Vec::new() } else { selector::parse_label_selector(label_selector)? };
    let field_reqs = if field_selector.is_empty() { Vec::new() } else { selector::parse_field_selector(field_selector)? };

    let prefix = keys::list_prefix(group, resource, namespace).into_bytes();
    let range_end = prefix_range_end(&prefix);
    let resp = storage.range(RangeRequest { key: prefix, range_end, ..Default::default() }).await?;
    let revision = resp.header.map(|h| h.revision).unwrap_or(0);
    let items = resp
        .kvs
        .iter()
        .map(|kv| decode_stored_object(&kv.value))
        .collect::<Result<Vec<Value>, protobuf::Error>>()?
        .into_iter()
        .filter(|item| selector::object_matches(item, &label_reqs, &field_reqs))
        .collect::<Vec<Value>>();

    let group_version = if group.is_empty() { version.to_string() } else { format!("{group}/{version}") };
    Ok(ListOutcome::Found(json!({
        "kind": list_kind(kind),
        "apiVersion": group_version,
        "metadata": {"resourceVersion": revision.to_string()},
        "items": items,
    })))
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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
