//! Converts a `cacher::store::WatchEvent` into the real wire shape a
//! `WATCH` response streams — `metav1.WatchEvent{Type, Object}`
//! (`staging/src/k8s.io/apimachinery/pkg/apis/meta/v1/watch.go`, fetched
//! and read directly): `{"type": "ADDED"|"MODIFIED"|"DELETED"|"BOOKMARK",
//! "object": {...}}`, `Type` values from
//! `staging/src/k8s.io/apimachinery/pkg/watch/watch.go`'s own
//! `EventType` constants.
//!
//! **Wired into a real streaming HTTP response** —
//! `server::listener::watch_response_body` encodes every event through
//! [`to_watch_event_json`] as a newline-terminated JSON document (this
//! doc comment used to say neither existed yet; both have for a while).
//!
//! `Deleted` events carry the real last-known object state: real
//! upstream's own `WatchEvent.Object` doc comment says a `Deleted`
//! event's `Object` is "the state of the object immediately before
//! deletion", and `cacher::store::WatchCache::apply` now retains exactly
//! that (a real, previously-named gap — fixed there, not in this module,
//! since it's the cache's own job to remember it). [`to_watch_event_json`]
//! still returns `None` for the one honest edge case that can still
//! happen (a `Deleted` event for a key this cache never held a value
//! for, e.g. right at a relist boundary) rather than fabricating a
//! placeholder object with no real spec/status data.

use crate::cacher::store::{EventKind, WatchEvent};
use crate::server::rest::{decode_stored_object, decrypt_and_decode, Error};
use crate::storage::client::StorageClient;
use serde_json::{json, Value};

/// Decodes `bytes` (the value half of `event`), decrypting first when
/// `storage` is given and has a matching transformer for `(group,
/// resource)` — Group C's encryption-at-rest, wired into `WATCH` the
/// same way every other real read path in this crate is
/// (`server::rest::decrypt_and_decode`, shared code, not a separate
/// reimplementation here). `storage: None` (matching this crate's own
/// "fail open on missing infrastructure, not on failed decryption"
/// posture used elsewhere — e.g. `flowcontrol::resolve`'s own doc
/// comment) falls back to a plain decode, same as before this wiring
/// existed; a real event should never be hidden because there was no
/// storage handle to check for a transformer.
fn decode_event_value(event: &WatchEvent, bytes: &[u8], storage: Option<&StorageClient>, group: &str, resource: &str) -> Result<Value, Error> {
    match storage {
        Some(s) => decrypt_and_decode(s, group, resource, &event.key, bytes),
        None => Ok(decode_stored_object(bytes)?),
    }
}

/// Real, load-bearing fix, found live the same way `server::rest::get`'s
/// own copy of this exact fix was — `tests/apiservice_roundtrip.rs`'s
/// get-then-update round trip: `resourceVersion` is never actually
/// *persisted* into a stored object's own bytes (every write path stamps
/// it onto its own return value only *after* the write that produced the
/// revision, since it doesn't exist yet while those bytes are still
/// being built), so a `Added`/`Modified`/`Deleted` watch event's own
/// decoded object needs it stamped from `event.revision` the same way
/// every real read path in `server::rest` now does, or a controller that
/// only ever watches (never a plain `GET`) would see every object with
/// no `resourceVersion` at all.
fn stamp_resource_version(object: &mut Value, revision: i64) {
    // Same shape `server::rest::set_metadata_field` already establishes
    // (not imported directly -- that one's private to its own module,
    // and this is the only field this module itself ever needs to set).
    let Some(map) = object.as_object_mut() else { return };
    let metadata = map.entry("metadata").or_insert_with(|| json!({}));
    if !metadata.is_object() {
        *metadata = json!({});
    }
    metadata["resourceVersion"] = Value::String(revision.to_string());
}

/// Stored protobuf values omit the top-level type envelope.  Put the type
/// metadata back on every object emitted on the watch wire; clients such as
/// flannel decode it as a Kubernetes object rather than as an untyped JSON
/// document.
fn stamp_type_metadata(object: &mut Value, kind: &str, api_version: &str) {
    let Some(map) = object.as_object_mut() else { return };
    map.insert("kind".to_string(), Value::String(kind.to_string()));
    map.insert("apiVersion".to_string(), Value::String(api_version.to_string()));
}

/// Real upstream's own `watch.EventType` string constants.
pub fn event_type_str(kind: EventKind) -> &'static str {
    match kind {
        EventKind::Added => "ADDED",
        EventKind::Modified => "MODIFIED",
        EventKind::Deleted => "DELETED",
        EventKind::Bookmark => "BOOKMARK",
    }
}

/// `kind`/`api_version` are the resource's own — a watch is always
/// scoped to one resource type, so unlike a decoded object's envelope
/// (which carries its own `apiVersion`/`kind`), a `Bookmark` event (which
/// has no stored value to read them from at all) needs them supplied by
/// the caller, who already knows what resource this watch is for.
///
/// Decoded Added/Modified/Deleted objects have their `kind` and `apiVersion`
/// restored from the watch's resource scope because those fields are not
/// persisted in the protobuf message itself.
///
/// `None` for a `Deleted` event with no retained value (the one case
/// `WatchCache` itself can't retain a value for — see this module's own
/// doc comment) — an honest `None`, not a bug papered over with an
/// invented placeholder object.
pub fn to_watch_event_json(event: &WatchEvent, kind: &str, api_version: &str, storage: Option<&StorageClient>, group: &str, resource: &str) -> Option<Result<Value, Error>> {
    let event_type = event_type_str(event.kind);
    match event.kind {
        EventKind::Added | EventKind::Modified => {
            let mut object = match decode_event_value(event, &event.value, storage, group, resource) {
                Ok(o) => o,
                Err(e) => return Some(Err(e)),
            };
            stamp_type_metadata(&mut object, kind, api_version);
            stamp_resource_version(&mut object, event.revision);
            Some(Ok(json!({"type": event_type, "object": object})))
        }
        EventKind::Bookmark => Some(Ok(json!({
            "type": event_type,
            "object": {
                "kind": kind,
                "apiVersion": api_version,
                "metadata": {"resourceVersion": event.revision.to_string()},
            },
        }))),
        EventKind::Deleted => {
            if event.value.is_empty() {
                None
            } else {
                // The common case now: `WatchCache::apply` retains the
                // pre-delete value, so this is what a real Deleted event
                // actually carries.
                match decode_event_value(event, &event.value, storage, group, resource) {
                    Ok(mut o) => {
                        stamp_type_metadata(&mut o, kind, api_version);
                        stamp_resource_version(&mut o, event.revision);
                        Some(Ok(json!({"type": event_type, "object": o})))
                    }
                    Err(e) => Some(Err(e)),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::protobuf;

    fn added_event(value: Vec<u8>, revision: i64) -> WatchEvent {
        WatchEvent { kind: EventKind::Added, key: b"/registry/namespaces/default".to_vec(), value, revision }
    }

    fn real_namespace_envelope(name: &str) -> Vec<u8> {
        let schema = protobuf::schema_for_gvk("", "v1", "Namespace").unwrap();
        let object_bytes = protobuf::encode_message(schema, &json!({"metadata": {"name": name}})).unwrap();
        protobuf::wrap_unknown("v1", "Namespace", &object_bytes)
    }

    #[test]
    fn added_and_modified_decode_the_real_stored_object() {
        let envelope = real_namespace_envelope("default");
        for kind in [EventKind::Added, EventKind::Modified] {
            let event = WatchEvent { kind, key: b"k".to_vec(), value: envelope.clone(), revision: 7 };
            let json = to_watch_event_json(&event, "Namespace", "v1", None, "", "namespaces").expect("Added/Modified must always convert").expect("decode must succeed");
            assert_eq!(json["type"], if kind == EventKind::Added { "ADDED" } else { "MODIFIED" });
            assert_eq!(json["object"]["metadata"]["name"], "default");
            assert_eq!(json["object"]["kind"], "Namespace");
            assert_eq!(json["object"]["apiVersion"], "v1");
        }
    }

    #[test]
    fn bookmark_carries_only_kind_apiversion_and_resource_version() {
        let event = WatchEvent { kind: EventKind::Bookmark, key: Vec::new(), value: Vec::new(), revision: 42 };
        let json = to_watch_event_json(&event, "Pod", "v1", None, "", "pods").unwrap().unwrap();
        assert_eq!(json["type"], "BOOKMARK");
        assert_eq!(json["object"]["kind"], "Pod");
        assert_eq!(json["object"]["apiVersion"], "v1");
        assert_eq!(json["object"]["metadata"]["resourceVersion"], "42");
    }

    #[test]
    fn deleted_with_no_retained_value_is_a_named_none_not_a_fabrication() {
        let event = WatchEvent { kind: EventKind::Deleted, key: b"k".to_vec(), value: Vec::new(), revision: 9 };
        assert!(to_watch_event_json(&event, "Pod", "v1", None, "", "pods").is_none());
    }

    #[test]
    fn deleted_with_a_real_value_decodes_it_honestly() {
        let envelope = real_namespace_envelope("goner");
        let event = WatchEvent { kind: EventKind::Deleted, key: b"k".to_vec(), value: envelope, revision: 9 };
        let json = to_watch_event_json(&event, "Namespace", "v1", None, "", "namespaces").unwrap().unwrap();
        assert_eq!(json["type"], "DELETED");
        assert_eq!(json["object"]["metadata"]["name"], "goner");
    }

    /// Real, load-bearing fix — see `stamp_resource_version`'s own doc
    /// comment: `resourceVersion` is never actually persisted into a
    /// stored object's own bytes, so every real event kind that carries
    /// a decoded object needs it stamped from the event's own revision,
    /// not just the synthetic `Bookmark` case (which already had this).
    #[test]
    fn added_modified_and_deleted_all_stamp_the_events_own_revision_as_resource_version() {
        let envelope = real_namespace_envelope("default");
        for kind in [EventKind::Added, EventKind::Modified, EventKind::Deleted] {
            let event = WatchEvent { kind, key: b"k".to_vec(), value: envelope.clone(), revision: 123 };
            let json = to_watch_event_json(&event, "Namespace", "v1", None, "", "namespaces").expect("must convert").expect("decode must succeed");
            assert_eq!(json["object"]["metadata"]["resourceVersion"], "123", "{kind:?} event must carry its own real resourceVersion");
            assert_eq!(json["object"]["kind"], "Namespace");
            assert_eq!(json["object"]["apiVersion"], "v1");
        }
    }

    #[test]
    fn a_corrupt_stored_value_is_a_real_error_not_a_panic() {
        let event = added_event(b"not a real envelope".to_vec(), 1);
        let result = to_watch_event_json(&event, "Namespace", "v1", None, "", "namespaces").expect("Added must attempt a decode");
        assert!(result.is_err());
    }
}
