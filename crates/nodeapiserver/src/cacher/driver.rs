//! Wires `storage::client::StorageClient` (a real nodestore connection) to
//! `cacher::store::WatchCache` (the in-memory core): LIST for a snapshot +
//! RV, then WATCH from `RV + 1`, applying every event as it arrives —
//! `ARCHITECTURE.md` §4's "LIST for a point-in-time RV, then WATCH from
//! it" made real.
//!
//! Split into pure decode logic (`decode_event`, `apply_watch_response` —
//! unit-tested against constructed `mvccpb`/`etcdserverpb` values, no
//! network needed) and thin async orchestration (`list`, `watch_from_list`)
//! that only wraps `StorageClient` calls — the same split every group so
//! far has kept between what a unit test can prove and what genuinely
//! needs live infrastructure.
//!
//! **Not yet done**, named honestly rather than left unsaid: a
//! reconnect-on-disconnect loop (a caller today gets one LIST-then-WATCH
//! cycle and must notice the response stream ending and start over itself)
//! and bookmark *generation* on a timer — this module only turns a
//! progress-notify response nodestore might send into a cache bookmark, it
//! doesn't request one on any schedule.

use crate::cacher::store::{CacheEntry, EventKind, WatchCache};
use crate::storage::client::{prefix_range_end, Error as StorageError, StorageClient, WatchHandle};
use crate::storage::pb::etcdserverpb::{RangeRequest, WatchResponse};
use crate::storage::pb::mvccpb;

/// LIST: fetch every key under `key_prefix` at the current revision.
/// Returns the seed data and revision `WatchCache::new` needs.
pub async fn list(client: &mut StorageClient, key_prefix: &[u8]) -> Result<(Vec<(Vec<u8>, CacheEntry)>, i64), StorageError> {
    let req = RangeRequest {
        key: key_prefix.to_vec(),
        range_end: prefix_range_end(key_prefix),
        ..Default::default()
    };
    let resp = client.range(req).await?;
    let revision = resp.header.map(|h| h.revision).unwrap_or(0);
    let items = resp
        .kvs
        .into_iter()
        .map(|kv| (kv.key, CacheEntry { value: kv.value, mod_revision: kv.mod_revision }))
        .collect();
    Ok((items, revision))
}

/// WATCH: opens a watcher over `key_prefix` starting just past `cache`'s
/// current revision — not at it, so the event that produced the LIST
/// snapshot's own revision isn't redelivered (which would otherwise
/// violate `WatchCache::apply`'s monotonic-increase expectation the moment
/// the very first watch response arrived).
pub async fn watch_from_cache(client: &mut StorageClient, key_prefix: &[u8], cache: &WatchCache) -> Result<WatchHandle, StorageError> {
    client.watch(key_prefix.to_vec(), prefix_range_end(key_prefix), cache.revision() + 1).await
}

/// Applies every event in one `WatchResponse` to `cache`. An empty
/// `events` list with a header revision past what the cache has already
/// seen is treated as a progress notification and turned into a
/// [`EventKind::Bookmark`] — real kube-apiserver sends these so a watcher
/// that reconnects can resume from a recent RV without a full relist
/// (`ARCHITECTURE.md` §4); nodestore's own `progress_notify` mechanism is
/// what a real driver loop would request on a timer to get one
/// periodically (the "not yet done" named in this module's own doc
/// comment). The `created`/`canceled` acknowledgement responses carry no
/// events and no revision advance worth applying, so they're a no-op here.
pub fn apply_watch_response(cache: &mut WatchCache, resp: &WatchResponse) {
    let header_revision = resp.header.as_ref().map(|h| h.revision).unwrap_or(0);
    if resp.events.is_empty() {
        if header_revision > cache.revision() {
            cache.apply(EventKind::Bookmark, Vec::new(), Vec::new(), header_revision);
        }
        return;
    }
    for event in &resp.events {
        let Some((kind, key, value, revision)) = decode_event(event) else { continue };
        // Defensive against any overlap between the LIST snapshot and the
        // watch's own start_revision boundary — see watch_from_cache's own
        // doc comment for why +1 should already prevent this in practice.
        if revision <= cache.revision() {
            continue;
        }
        cache.apply(kind, key, value, revision);
    }
}

/// Decodes one `mvccpb::Event` into what `WatchCache::apply` needs.
/// `None` for an event with no `kv` at all — malformed/defensive, not a
/// real case nodestore's own server produces (`event_to_pb` in
/// `crates/nodestore/src/server/convert.rs` always sets `kv`).
///
/// `Added` vs `Modified` isn't a distinct wire concept — both are a `PUT`
/// `EventType`. Distinguished the same way `mvccpb::Event`'s own doc
/// comment says to: `kv.version == 1` means creation.
fn decode_event(event: &mvccpb::Event) -> Option<(EventKind, Vec<u8>, Vec<u8>, i64)> {
    let kv = event.kv.as_ref()?;
    let kind = if event.r#type == mvccpb::event::EventType::Delete as i32 {
        EventKind::Deleted
    } else if kv.version == 1 {
        EventKind::Added
    } else {
        EventKind::Modified
    };
    let value = if kind == EventKind::Deleted { Vec::new() } else { kv.value.clone() };
    Some((kind, kv.key.clone(), value, kv.mod_revision))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::pb::etcdserverpb::ResponseHeader;
    use crate::storage::pb::mvccpb::{Event, KeyValue};

    fn put_event(key: &str, value: &str, version: i64, mod_revision: i64) -> Event {
        Event {
            r#type: mvccpb::event::EventType::Put as i32,
            kv: Some(KeyValue {
                key: key.as_bytes().to_vec(),
                value: value.as_bytes().to_vec(),
                version,
                mod_revision,
                create_revision: 1,
                lease: 0,
            }),
            prev_kv: None,
        }
    }

    fn delete_event(key: &str, mod_revision: i64) -> Event {
        Event {
            r#type: mvccpb::event::EventType::Delete as i32,
            kv: Some(KeyValue { key: key.as_bytes().to_vec(), mod_revision, ..Default::default() }),
            prev_kv: None,
        }
    }

    fn watch_response(events: Vec<Event>, header_revision: i64) -> WatchResponse {
        WatchResponse {
            header: Some(ResponseHeader { revision: header_revision, ..Default::default() }),
            events,
            ..Default::default()
        }
    }

    #[test]
    fn decode_event_treats_version_one_put_as_added() {
        let (kind, key, value, rev) = decode_event(&put_event("a", "v1", 1, 5)).unwrap();
        assert_eq!(kind, EventKind::Added);
        assert_eq!(key, b"a");
        assert_eq!(value, b"v1");
        assert_eq!(rev, 5);
    }

    #[test]
    fn decode_event_treats_version_above_one_put_as_modified() {
        let (kind, ..) = decode_event(&put_event("a", "v2", 2, 6)).unwrap();
        assert_eq!(kind, EventKind::Modified);
    }

    #[test]
    fn decode_event_treats_delete_as_deleted_with_no_value() {
        let (kind, key, value, rev) = decode_event(&delete_event("a", 7)).unwrap();
        assert_eq!(kind, EventKind::Deleted);
        assert_eq!(key, b"a");
        assert!(value.is_empty());
        assert_eq!(rev, 7);
    }

    #[test]
    fn decode_event_returns_none_for_a_missing_kv() {
        let event = Event { r#type: mvccpb::event::EventType::Put as i32, kv: None, prev_kv: None };
        assert!(decode_event(&event).is_none());
    }

    #[test]
    fn apply_watch_response_applies_every_event_in_order() {
        let mut cache = WatchCache::new(vec![], 1, 16, 16);
        let resp = watch_response(vec![put_event("a", "v1", 1, 2), put_event("b", "v1", 1, 3)], 3);
        apply_watch_response(&mut cache, &resp);
        assert_eq!(cache.revision(), 3);
        assert_eq!(cache.list().0.len(), 2);
    }

    #[test]
    fn apply_watch_response_with_no_events_but_a_newer_header_is_a_bookmark() {
        let mut cache = WatchCache::new(vec![(b"a".to_vec(), CacheEntry { value: b"v".to_vec(), mod_revision: 1 })], 1, 16, 16);
        let resp = watch_response(vec![], 9);
        apply_watch_response(&mut cache, &resp);
        assert_eq!(cache.revision(), 9, "a progress notification must still advance the cache's revision");
        assert_eq!(cache.list().0.len(), 1, "a bookmark must not touch any key");
    }

    #[test]
    fn apply_watch_response_with_no_events_and_no_newer_header_is_a_true_no_op() {
        // The created:true acknowledgement response, or a stale progress
        // notification — neither should touch the cache at all.
        let mut cache = WatchCache::new(vec![], 5, 16, 16);
        let resp = WatchResponse { header: Some(ResponseHeader { revision: 5, ..Default::default() }), created: true, ..Default::default() };
        apply_watch_response(&mut cache, &resp);
        assert_eq!(cache.revision(), 5);
    }

    #[test]
    fn apply_watch_response_skips_an_event_at_or_below_the_caches_own_revision() {
        // Defends the boundary watch_from_cache's own doc comment
        // describes: even if a client sent (or a server delivered) an
        // event that overlaps what the LIST snapshot already reflects,
        // applying it again must not panic or double-count it.
        let mut cache = WatchCache::new(vec![], 5, 16, 16);
        let resp = watch_response(vec![put_event("a", "v", 1, 5)], 5);
        apply_watch_response(&mut cache, &resp);
        assert_eq!(cache.revision(), 5, "an at-or-below-current event must be skipped, not applied");
        assert!(cache.list().0.is_empty());
    }
}
