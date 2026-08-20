//! Group E: the first real, generic REST verb — single-object `GET` —
//! wired against actual nodestore data. Closes the gap `docs/APISERVER.md`
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
//! `GET` (single object, `GET /api/v1/namespaces/{ns}/pods/{name}`-shaped)
//! and `LIST` (`GET /api/v1/namespaces/{ns}/pods`-shaped, no name) —
//! `watch`/`create`/`update`/`patch`/`delete` all remain the bring-up echo
//! stub (`server::listener`'s own doc comment). Reads go straight to
//! nodestore via `storage::client::StorageClient::range`, bypassing
//! `cacher::store::WatchCache` entirely — a real, valid read strategy
//! (upstream's own quorum-read / watch-cache-disabled path takes exactly
//! this shape), not a shortcut standing in for the cache; wiring the
//! cache in is separate follow-up work, blocked on `cacher::driver`'s own
//! reconnect loop actually being started at boot (today nothing in
//! `lib.rs::run()` calls it). No authentication, no authorization, no
//! admission — every request reaching this function is currently treated
//! as allowed, the same "deliberately incomplete but honest bring-up
//! milestone" posture `server::tls`'s own doc comment already established
//! for this crate's self-signed cert (not the cluster's real PKI either).
//! Groups H/I/J replace this before anything here is production-usable.
//! Subresources (`pods/status`, `pods/log`, ...) aren't handled — the
//! discovery table this module reads doesn't carry them either (a named,
//! separate skip in `build/discovery_parse.rs`). `list` filters by
//! label/field selector for real (`cacher::selector::object_matches`,
//! wired against every item's own decoded JSON — Group D's own generic
//! adapter, unchanged here). `list`'s remaining real gaps: no pagination
//! (`continue`/`limit`), no `resourceVersion`-pinned reads (always reads
//! at the current revision).

use crate::cacher::selector::{self, ParseError};
use crate::codec::protobuf;
use crate::codegen;
use crate::storage::client::{prefix_range_end, Error as StorageError, StorageClient};
use crate::storage::keys;
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
}
