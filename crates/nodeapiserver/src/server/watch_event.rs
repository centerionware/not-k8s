//! Converts a `cacher::store::WatchEvent` into the real wire shape a
//! `WATCH` response streams — `metav1.WatchEvent{Type, Object}`
//! (`staging/src/k8s.io/apimachinery/pkg/apis/meta/v1/watch.go`, fetched
//! and read directly): `{"type": "ADDED"|"MODIFIED"|"DELETED"|"BOOKMARK",
//! "object": {...}}`, `Type` values from
//! `staging/src/k8s.io/apimachinery/pkg/watch/watch.go`'s own
//! `EventType` constants.
//!
//! **Pure conversion only — not yet wired into an actual streaming HTTP
//! response.** `server::listener` has no long-lived, chunked-response
//! machinery yet, and nothing in `lib.rs::run()` starts a
//! `cacher::registry::CacheRegistry` to read events from in the first
//! place; both are separate, not-yet-started work.
//!
//! # Named gap: `Deleted` events have no last-known object state
//!
//! Real upstream's own `WatchEvent.Object` doc comment: for a `Deleted`
//! event, `Object` is "the state of the object immediately before
//! deletion." `cacher::store::WatchEvent` doesn't retain this — its own
//! doc comment states `value` is empty for `Deleted`/`Bookmark` — a real,
//! pre-existing gap in Group D's own cache design, not introduced here.
//! Rather than fabricate a placeholder object with no real spec/status
//! data (which would be actively misleading to a real client, worse than
//! an honest gap), [`to_watch_event_json`] returns `None` for a `Deleted`
//! event with no value: it needs `WatchCache` to start retaining the last
//! value on delete, separate, not-yet-started work.

use crate::cacher::store::{EventKind, WatchEvent};
use crate::server::rest::decode_stored_object;
use serde_json::{json, Value};

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
/// `None` for a `Deleted` event with no retained value — see this
/// module's own doc comment for why that's an honest gap, not a bug to
/// paper over with an invented placeholder object.
pub fn to_watch_event_json(event: &WatchEvent, kind: &str, api_version: &str) -> Option<Result<Value, crate::codec::protobuf::Error>> {
    let event_type = event_type_str(event.kind);
    match event.kind {
        EventKind::Added | EventKind::Modified => {
            let object = match decode_stored_object(&event.value) {
                Ok(o) => o,
                Err(e) => return Some(Err(e)),
            };
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
                // Real upstream's own case — this cache doesn't
                // currently produce a Deleted event with a non-empty
                // value, but honor it correctly if that ever changes
                // rather than assuming it can't.
                match decode_stored_object(&event.value) {
                    Ok(o) => Some(Ok(json!({"type": event_type, "object": o}))),
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
            let json = to_watch_event_json(&event, "Namespace", "v1").expect("Added/Modified must always convert").expect("decode must succeed");
            assert_eq!(json["type"], if kind == EventKind::Added { "ADDED" } else { "MODIFIED" });
            assert_eq!(json["object"]["metadata"]["name"], "default");
        }
    }

    #[test]
    fn bookmark_carries_only_kind_apiversion_and_resource_version() {
        let event = WatchEvent { kind: EventKind::Bookmark, key: Vec::new(), value: Vec::new(), revision: 42 };
        let json = to_watch_event_json(&event, "Pod", "v1").unwrap().unwrap();
        assert_eq!(json["type"], "BOOKMARK");
        assert_eq!(json["object"]["kind"], "Pod");
        assert_eq!(json["object"]["apiVersion"], "v1");
        assert_eq!(json["object"]["metadata"]["resourceVersion"], "42");
    }

    #[test]
    fn deleted_with_no_retained_value_is_a_named_none_not_a_fabrication() {
        let event = WatchEvent { kind: EventKind::Deleted, key: b"k".to_vec(), value: Vec::new(), revision: 9 };
        assert!(to_watch_event_json(&event, "Pod", "v1").is_none());
    }

    #[test]
    fn deleted_with_a_real_value_decodes_it_honestly() {
        let envelope = real_namespace_envelope("goner");
        let event = WatchEvent { kind: EventKind::Deleted, key: b"k".to_vec(), value: envelope, revision: 9 };
        let json = to_watch_event_json(&event, "Namespace", "v1").unwrap().unwrap();
        assert_eq!(json["type"], "DELETED");
        assert_eq!(json["object"]["metadata"]["name"], "goner");
    }

    #[test]
    fn a_corrupt_stored_value_is_a_real_error_not_a_panic() {
        let event = added_event(b"not a real envelope".to_vec(), 1);
        let result = to_watch_event_json(&event, "Namespace", "v1").expect("Added must attempt a decode");
        assert!(result.is_err());
    }
}
