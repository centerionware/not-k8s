//! The in-memory cache core: an ordered snapshot of the latest value per
//! key, plus a bounded recent-event history watchers replay from. Pure and
//! synchronous underneath (`apply`/`list`/`events_since` take no lock
//! across an await point) — network I/O (LIST-then-WATCH against
//! `storage::client::StorageClient`, reconnect-on-disconnect) is a
//! separate driver loop this module doesn't implement yet, named honestly
//! in `cacher/mod.rs`'s own doc comment rather than left unsaid.
//!
//! Mirrors real kube-apiserver's own cacher in shape (`ARCHITECTURE.md`
//! §4, `docs/APISERVER_PLAN.md` finding 3): LIST for a point-in-time RV,
//! then WATCH from it, served from memory. Simpler than upstream's,
//! per that finding — nodestore hands applied events straight to watchers
//! with no polling, so this cache exists for *semantics* (RV=0 reads,
//! consistent reads, watch replay), not to shield the backend from load.

use std::collections::{BTreeMap, VecDeque};
use tokio::sync::{broadcast, watch, Notify};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventKind {
    Added,
    Modified,
    Deleted,
    /// A synthetic no-op event that only advances `revision` — real
    /// kube-apiserver sends these so a watcher whose connection blips can
    /// resume from a recent RV without a full relist (`ARCHITECTURE.md`
    /// §4). `key`/`value` are always empty on a `Bookmark`.
    Bookmark,
}

#[derive(Debug, Clone)]
pub struct WatchEvent {
    pub kind: EventKind,
    pub key: Vec<u8>,
    /// The object's encoded value — for `Deleted`, this is the last
    /// known value *before* deletion (real upstream's own
    /// `WatchEvent.Object` semantics for a delete: "the state of the
    /// object immediately before deletion" — see [`WatchCache::apply`]'s
    /// own doc comment for how that's captured), empty only if this
    /// cache never held a value for the deleted key at all. Always empty
    /// for `Bookmark`.
    pub value: Vec<u8>,
    pub revision: i64,
}

#[derive(Debug, Clone)]
pub struct CacheEntry {
    pub value: Vec<u8>,
    pub mod_revision: i64,
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The requested `start_revision` is older than anything this cache
    /// still retains — the real-etcd/real-kube-apiserver "410 Gone"
    /// case. The caller's only correct move is a fresh LIST (to get a
    /// current snapshot + RV) and a new watch from there, same as any
    /// real `client-go` informer does on this exact error.
    #[error("requested revision {requested} is older than the oldest retained ({oldest}) — relist required")]
    TooOld { requested: i64, oldest: i64 },
}

type Result<T> = std::result::Result<T, Error>;

/// One resource's watch cache. Not `Clone`, not `Sync`-friendly to share
/// bare — every method here is `&self`/`&mut self` with no internal
/// locking, on purpose (keeps this type trivially unit-testable with no
/// async runtime needed for the pure logic). A driver loop that calls
/// [`Self::apply`] and reader tasks that call [`Self::list`]/
/// [`Self::watch_from`] concurrently need a lock around the whole thing —
/// [`SharedCache`] is that lock, and is what real callers should hold
/// instead of a bare `WatchCache`.
pub struct WatchCache {
    items: BTreeMap<Vec<u8>, CacheEntry>,
    /// Bounded ring of recent events, oldest first — how a watcher whose
    /// `start_revision` is behind `revision` but still within this window
    /// gets replayed without a relist. `history_limit` is a memory/replay-
    /// window tradeoff, not a correctness knob: too small just means more
    /// callers get [`Error::TooOld`] and fall back to a relist, same as
    /// real kube-apiserver's own cache size limit does.
    history: VecDeque<WatchEvent>,
    history_limit: usize,
    revision: i64,
    /// Broadcasts every applied event to live watchers. Bounded — a slow
    /// watcher that falls behind the channel's capacity gets `Lagged` from
    /// `broadcast::Receiver::recv()`, which its own driver loop turns into
    /// exactly the same relist path as [`Error::TooOld`], not a silent
    /// gap in what it sees.
    events: broadcast::Sender<WatchEvent>,
    /// Publishes `revision` on every apply, independent of `events`'
    /// capacity — this is what [`wait_for_revision`] (the free function
    /// below) waits on for consistent reads (`docs/APISERVER_PLAN.md` finding 3:
    /// "fetching the latest write revision from storage and waiting for
    /// the cache to reach it"), and it can never lag or drop a value the
    /// way a bounded channel could.
    revision_tx: watch::Sender<i64>,
}

impl WatchCache {
    /// Seeds the cache from a LIST snapshot at `revision` — the
    /// point-in-time RV a real LIST-then-WATCH init reads before starting
    /// its watch (`ARCHITECTURE.md` §4). `event_buffer` bounds the
    /// broadcast channel; `history_limit` bounds the relist-avoidance
    /// window described on [`Self::history`].
    pub fn new(initial: Vec<(Vec<u8>, CacheEntry)>, revision: i64, event_buffer: usize, history_limit: usize) -> WatchCache {
        let (events, _) = broadcast::channel(event_buffer.max(1));
        let (revision_tx, _) = watch::channel(revision);
        WatchCache {
            items: initial.into_iter().collect(),
            history: VecDeque::with_capacity(history_limit),
            history_limit,
            revision,
            events,
            revision_tx,
        }
    }

    pub fn revision(&self) -> i64 {
        self.revision
    }

    /// A cloneable, independently-owned handle on `revision` — the
    /// mechanism for a consistent read (finding 3) to wait for this cache
    /// to catch up *without* needing shared access to `WatchCache` itself,
    /// which is exclusively owned by whichever task calls [`Self::apply`]
    /// (a driver loop `&mut self`-borrowing `WatchCache` while a reader
    /// task also holds `&self` across an `await` is exactly the aliasing
    /// [`tokio::sync::watch::Receiver`] exists to avoid needing). Pass the
    /// result to the free function [`wait_for_revision`].
    pub fn revision_watch(&self) -> watch::Receiver<i64> {
        self.revision_tx.subscribe()
    }

    /// Applies one event from the live watch, in the order it arrived.
    /// `revision` must be monotonically increasing — the driver loop's
    /// job, not this method's, to guarantee (a real etcd/nodestore watch
    /// stream is already strictly ordered by construction, so this is a
    /// sanity assertion against a driver-loop bug, not a real-world input
    /// this needs to tolerate).
    pub fn apply(&mut self, kind: EventKind, key: Vec<u8>, value: Vec<u8>, revision: i64) {
        debug_assert!(revision > self.revision, "watch events must arrive in increasing revision order");
        // Real upstream's own `WatchEvent.Object` doc comment: for a
        // `Deleted` event, `Object` is "the state of the object
        // immediately before deletion." The caller (`cacher::driver`)
        // never has that value to hand (nodestore's own watch stream
        // isn't asked for `prev_kv`, and even if it were, decoding it
        // needs the resource's schema, which is `server`-layer, not
        // `cacher`-layer, knowledge) — but this cache already holds the
        // pre-delete value in `self.items` at the moment a delete
        // arrives, so it's captured here before the entry is removed,
        // rather than the caller's always-empty `value` argument being
        // used for a `Deleted` event. `None` (nothing retained for this
        // key — a delete for a key this cache never saw, e.g. right at a
        // relist boundary) falls back to empty, same as before this fix.
        let value = if kind == EventKind::Deleted { self.items.get(&key).map(|e| e.value.clone()).unwrap_or_default() } else { value };
        match kind {
            EventKind::Added | EventKind::Modified => {
                self.items.insert(key.clone(), CacheEntry { value: value.clone(), mod_revision: revision });
            }
            EventKind::Deleted => {
                self.items.remove(&key);
            }
            EventKind::Bookmark => {}
        }
        self.revision = revision;
        let event = WatchEvent { kind, key, value, revision };
        if self.history.len() == self.history_limit && self.history_limit > 0 {
            self.history.pop_front();
        }
        if self.history_limit > 0 {
            self.history.push_back(event.clone());
        }
        // No live receiver is not an error — a cache with nobody watching
        // yet still needs to keep its own state current.
        let _ = self.events.send(event);
        let _ = self.revision_tx.send(revision);
    }

    /// `resourceVersion=0`/unset semantics: whatever the cache currently
    /// holds, with no wait — the point of serving a LIST from the cache at
    /// all rather than always hitting storage (`ARCHITECTURE.md` §4).
    pub fn list(&self) -> (Vec<(Vec<u8>, CacheEntry)>, i64) {
        (self.items.iter().map(|(k, v)| (k.clone(), v.clone())).collect(), self.revision)
    }

    /// The single-key equivalent of [`Self::list`] — same "whatever the
    /// cache currently holds, no wait" semantics, for a GET rather than a
    /// LIST. `None` covers both "never existed" and "existed but was
    /// deleted"; this cache has no separate tombstone representation
    /// (`WatchCache::apply`'s own `EventKind::Deleted` arm removes the
    /// entry outright), matching a real `Range` request's own behavior
    /// for an absent key.
    pub fn get(&self, key: &[u8]) -> Option<CacheEntry> {
        self.items.get(key).cloned()
    }

    /// A live subscription plus the replay of any retained history the
    /// caller needs to catch up on if `start_revision` is behind
    /// `self.revision()`. `start_revision <= 0` means "start from now" —
    /// no replay, just the live subscription (matches
    /// `WatchCreateRequest.start_revision`'s own "no start_revision is
    /// now" semantics, reused here for the same reason `storage::client`'s
    /// `range()` reuses etcd's `revision <= 0` convention).
    ///
    /// [`Error::TooOld`] if `start_revision` is older than the oldest
    /// retained history entry — the caller's only correct response is a
    /// fresh LIST and a new `watch_from` at the RV that LIST returned.
    pub fn watch_from(&self, start_revision: i64) -> Result<(Vec<WatchEvent>, broadcast::Receiver<WatchEvent>)> {
        let rx = self.events.subscribe();
        if start_revision <= 0 {
            return Ok((Vec::new(), rx));
        }
        if let Some(oldest) = self.history.front() {
            if start_revision < oldest.revision {
                return Err(Error::TooOld { requested: start_revision, oldest: oldest.revision });
            }
        } else if start_revision < self.revision {
            // No history retained at all (history_limit == 0) but the
            // cache has moved past start_revision — nothing to replay
            // from, same conclusion as the oldest-entry check above.
            return Err(Error::TooOld { requested: start_revision, oldest: self.revision });
        }
        let replay: Vec<WatchEvent> = self.history.iter().filter(|e| e.revision > start_revision).cloned().collect();
        Ok((replay, rx))
    }
}

/// Blocks until `rx`'s current value is `>= target` — the second half of a
/// consistent read (finding 3): the caller fetches `target` from a fresh
/// direct storage read first, gets `rx` from
/// [`WatchCache::revision_watch`], then calls this to know the cache has
/// caught up before serving the LIST from it. A free function operating on
/// an owned/borrowed [`watch::Receiver`] rather than a `WatchCache` method,
/// deliberately — see [`WatchCache::revision_watch`]'s own doc comment for
/// why a `&self` method here would conflict with the driver loop's
/// exclusive `&mut self` ownership.
pub async fn wait_for_revision(rx: &mut watch::Receiver<i64>, target: i64) {
    if *rx.borrow() >= target {
        return;
    }
    while rx.changed().await.is_ok() {
        if *rx.borrow() >= target {
            return;
        }
    }
    // The sender (the owning WatchCache) was dropped — nothing left to
    // wait for; there is no further progress this call could ever see.
}

/// A [`WatchCache`] behind a lock, cheaply cloneable (an `Arc` clone) —
/// what a driver loop and its readers actually share. `std::sync::RwLock`,
/// not `tokio::sync::RwLock`: every `WatchCache` method is synchronous with
/// no `.await` inside it, so a guard is always released well before any
/// future would need to yield, and the standard library lock avoids the
/// async-lock overhead for a critical section this short. Multiple readers
/// ([`Self::list`], [`Self::watch_from`], [`Self::revision`],
/// [`Self::revision_watch`]) can hold a read lock concurrently; the driver
/// loop's [`Self::apply`] briefly takes a write lock per event.
///
/// Also carries `synced` — real `client-go`'s own `HasSynced()`, ported: a
/// fresh [`Self::new`] starts unsynced (the registered-but-empty case named
/// in `cacher::registry::CacheRegistry::spawn`'s own doc comment), and
/// [`Self::replace`] (what `cacher::driver::reflect`'s first successful
/// `LIST` calls) sets it permanently. This is a separate `AtomicBool`, not
/// "`revision() > 0`" — a genuinely empty resource that has, in fact,
/// completed its first `LIST` still reports revision `0` from nodestore
/// (an empty store has never advanced its revision), which would be
/// indistinguishable from "not synced yet" if synced-ness were inferred
/// from the revision alone. Exists so a reader (`server::rest::list`) can
/// tell "registered but still mid-relist" apart from "synced, genuinely
/// empty" — the same distinction [`Self::get`]'s own doc comment already
/// draws for the single-key case, but `list`'s cache consultation can't
/// borrow `get`'s "a miss always falls through" trick: an empty `Vec` is
/// itself a fully valid `LIST` answer (a real `200` with zero items), not
/// a miss to fall through on.
#[derive(Clone)]
pub struct SharedCache {
    inner: std::sync::Arc<std::sync::RwLock<WatchCache>>,
    synced: std::sync::Arc<std::sync::atomic::AtomicBool>,
    sync_notify: std::sync::Arc<Notify>,
}

impl SharedCache {
    pub fn new(cache: WatchCache) -> SharedCache {
        SharedCache {
            inner: std::sync::Arc::new(std::sync::RwLock::new(cache)),
            synced: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            sync_notify: std::sync::Arc::new(Notify::new()),
        }
    }

    fn read(&self) -> std::sync::RwLockReadGuard<'_, WatchCache> {
        self.inner.read().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn write(&self) -> std::sync::RwLockWriteGuard<'_, WatchCache> {
        self.inner.write().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Whether at least one [`Self::replace`] (a completed `LIST`) has
    /// happened yet — see this struct's own doc comment for why this is a
    /// real flag, not inferred from `revision()`. Once true, stays true:
    /// a later relist replaces the data, it doesn't un-sync the cache.
    pub fn has_synced(&self) -> bool {
        self.synced.load(std::sync::atomic::Ordering::Acquire)
    }

    /// Waits for the first successful reflector `LIST`. A watch opened
    /// before that snapshot is ready would otherwise subscribe to the
    /// temporary pre-sync cache and be closed when `replace()` installs the
    /// real snapshot.
    pub async fn wait_until_synced(&self) {
        loop {
            if self.has_synced() {
                return;
            }
            let notified = self.sync_notify.notified();
            if self.has_synced() {
                return;
            }
            notified.await;
        }
    }

    pub fn revision(&self) -> i64 {
        self.read().revision()
    }

    pub fn revision_watch(&self) -> watch::Receiver<i64> {
        self.read().revision_watch()
    }

    pub fn list(&self) -> (Vec<(Vec<u8>, CacheEntry)>, i64) {
        self.read().list()
    }

    pub fn get(&self, key: &[u8]) -> Option<CacheEntry> {
        self.read().get(key)
    }

    pub fn watch_from(&self, start_revision: i64) -> Result<(Vec<WatchEvent>, broadcast::Receiver<WatchEvent>)> {
        self.read().watch_from(start_revision)
    }

    /// Takes an atomic snapshot of the current cache and subscribes to
    /// subsequent events. The CRD lifecycle reconciler uses this to seed its
    /// registration map without a gap between the initial snapshot and the
    /// live watch.
    pub fn snapshot_and_watch(&self) -> (Vec<(Vec<u8>, CacheEntry)>, broadcast::Receiver<WatchEvent>) {
        let cache = self.read();
        let (items, _) = cache.list();
        (items, cache.events.subscribe())
    }

    pub fn apply(&self, kind: EventKind, key: Vec<u8>, value: Vec<u8>, revision: i64) {
        self.write().apply(kind, key, value, revision)
    }

    /// Swaps in an entirely new snapshot — what a reconnect-and-relist does
    /// after a watch stream ends or hits [`Error::TooOld`]: the old cache's
    /// data is stale/gone, so this replaces it wholesale rather than trying
    /// to reconcile it with a fresh LIST. Existing `watch_from`/
    /// `revision_watch` subscriptions on the *old* inner `WatchCache` are
    /// still valid (they hold their own `broadcast`/`watch` receiver
    /// clones, not a reference back into this `SharedCache`) but will never
    /// receive another event — same as a real watch ending and the client
    /// needing to call `watch_from` again against the new cache, which is
    /// exactly what a relist is.
    pub fn replace(&self, cache: WatchCache) {
        *self.write() = cache;
        self.synced.store(true, std::sync::atomic::Ordering::Release);
        self.sync_notify.notify_waiters();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(v: &str, rev: i64) -> CacheEntry {
        CacheEntry { value: v.as_bytes().to_vec(), mod_revision: rev }
    }

    #[test]
    fn list_reflects_the_seeded_snapshot() {
        let cache = WatchCache::new(vec![(b"a".to_vec(), entry("1", 5))], 5, 16, 16);
        let (items, rev) = cache.list();
        assert_eq!(rev, 5);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].0, b"a");
    }

    #[test]
    fn apply_add_modify_delete_updates_list_and_revision() {
        let mut cache = WatchCache::new(vec![], 1, 16, 16);
        cache.apply(EventKind::Added, b"a".to_vec(), b"v1".to_vec(), 2);
        assert_eq!(cache.list().0.len(), 1);
        assert_eq!(cache.revision(), 2);

        cache.apply(EventKind::Modified, b"a".to_vec(), b"v2".to_vec(), 3);
        let (items, _) = cache.list();
        assert_eq!(items[0].1.value, b"v2");

        cache.apply(EventKind::Deleted, b"a".to_vec(), Vec::new(), 4);
        assert!(cache.list().0.is_empty());
        assert_eq!(cache.revision(), 4);
    }

    #[test]
    fn a_deleted_event_carries_the_last_known_value_not_the_callers_empty_argument() {
        // Real upstream's own WatchEvent.Object semantics for a delete:
        // "the state of the object immediately before deletion". The
        // caller (cacher::driver, mirroring nodestore's own watch stream)
        // always passes an empty value for a Deleted event — WatchCache
        // itself is responsible for retaining the pre-delete value.
        // An unrelated seed event at revision 2 first, purely so
        // `watch_from`'s own "not older than the oldest retained history
        // entry" check has something at or before the requested
        // start_revision — `watch_from`'s semantics are unrelated to what
        // this test is proving and untouched by this fix.
        let mut cache = WatchCache::new(vec![(b"a".to_vec(), entry("last-value", 1))], 1, 16, 16);
        cache.apply(EventKind::Added, b"seed".to_vec(), b"unrelated".to_vec(), 2);
        cache.apply(EventKind::Deleted, b"a".to_vec(), Vec::new(), 3);
        let (replay, _rx) = cache.watch_from(2).unwrap();
        assert_eq!(replay.len(), 1);
        assert_eq!(replay[0].kind, EventKind::Deleted);
        assert_eq!(replay[0].value, b"last-value", "the Deleted WatchEvent must carry the value the key held just before deletion");
    }

    #[test]
    fn deleting_a_key_this_cache_never_held_a_value_for_falls_back_to_empty() {
        // A delete for a key this cache never saw (e.g. right at a relist
        // boundary) has nothing to retain — falls back to empty rather
        // than panicking or fabricating a value.
        let mut cache = WatchCache::new(vec![], 1, 16, 16);
        cache.apply(EventKind::Added, b"seed".to_vec(), b"unrelated".to_vec(), 2);
        cache.apply(EventKind::Deleted, b"never-seen".to_vec(), Vec::new(), 3);
        let (replay, _rx) = cache.watch_from(2).unwrap();
        assert_eq!(replay[0].value, Vec::<u8>::new());
    }

    #[test]
    fn get_finds_a_seeded_key_and_reflects_apply() {
        let mut cache = WatchCache::new(vec![(b"a".to_vec(), entry("1", 5))], 5, 16, 16);
        assert_eq!(cache.get(b"a").unwrap().value, b"1");
        assert!(cache.get(b"missing").is_none());

        cache.apply(EventKind::Modified, b"a".to_vec(), b"2".to_vec(), 6);
        assert_eq!(cache.get(b"a").unwrap().value, b"2");

        cache.apply(EventKind::Deleted, b"a".to_vec(), Vec::new(), 7);
        assert!(cache.get(b"a").is_none(), "a deleted key must not still be found");
    }

    #[test]
    fn shared_cache_get_delegates_to_the_inner_cache() {
        let shared = SharedCache::new(WatchCache::new(vec![(b"a".to_vec(), entry("1", 5))], 5, 16, 16));
        assert_eq!(shared.get(b"a").unwrap().value, b"1");
        assert!(shared.get(b"missing").is_none());
    }

    #[test]
    fn a_fresh_shared_cache_has_not_synced() {
        let shared = SharedCache::new(WatchCache::new(vec![], 0, 16, 16));
        assert!(!shared.has_synced());
    }

    #[test]
    fn replace_marks_the_cache_synced_even_on_a_genuinely_empty_list() {
        // The case this flag exists for: an empty LIST result is a real,
        // valid completed sync, not distinguishable from "not synced yet"
        // by revision alone (see SharedCache's own doc comment).
        let shared = SharedCache::new(WatchCache::new(vec![], 0, 16, 16));
        assert!(!shared.has_synced());
        shared.replace(WatchCache::new(vec![], 0, 16, 16));
        assert!(shared.has_synced());
    }

    #[test]
    fn has_synced_stays_true_across_a_later_relist() {
        let shared = SharedCache::new(WatchCache::new(vec![], 0, 16, 16));
        shared.replace(WatchCache::new(vec![(b"a".to_vec(), entry("1", 5))], 5, 16, 16));
        assert!(shared.has_synced());
        shared.replace(WatchCache::new(vec![], 9, 16, 16));
        assert!(shared.has_synced(), "a relist must not un-sync the cache");
    }

    #[tokio::test]
    async fn wait_for_revision_returns_immediately_if_already_caught_up() {
        let cache = WatchCache::new(vec![], 5, 16, 16);
        let mut rx = cache.revision_watch();
        tokio::time::timeout(std::time::Duration::from_millis(100), wait_for_revision(&mut rx, 3))
            .await
            .expect("must not block when already at or past the target");
    }

    /// Genuinely concurrent, not just sequential-then-await: the waiter
    /// task is spawned (and confirmed still pending) *before* `apply`
    /// moves the revision forward, so this actually exercises the
    /// `watch::Receiver`'s wake path rather than observing an
    /// already-satisfied condition. `revision_watch()` only needs `&self`
    /// momentarily to subscribe, so it coexists with the driver loop's
    /// later `&mut self` `apply()` calls exactly as the design intends.
    #[tokio::test]
    async fn wait_for_revision_unblocks_once_apply_reaches_the_target() {
        let mut cache = WatchCache::new(vec![], 1, 16, 16);
        let mut rx = cache.revision_watch();
        let waiter = tokio::spawn(async move {
            wait_for_revision(&mut rx, 5).await;
        });

        // Give the spawned task a chance to start waiting before the
        // target is reached, so this is a real "wait, then wake" rather
        // than a race that happens to pass.
        tokio::task::yield_now().await;
        assert!(!waiter.is_finished(), "must still be waiting before revision 5 is applied");

        cache.apply(EventKind::Added, b"a".to_vec(), b"v".to_vec(), 5);
        tokio::time::timeout(std::time::Duration::from_millis(100), waiter)
            .await
            .expect("must unblock once the target revision is reached")
            .unwrap();
    }

    #[test]
    fn watch_from_zero_or_negative_means_start_from_now_with_no_replay() {
        let mut cache = WatchCache::new(vec![], 1, 16, 16);
        cache.apply(EventKind::Added, b"a".to_vec(), b"v".to_vec(), 2);
        let (replay, _rx) = cache.watch_from(0).unwrap();
        assert!(replay.is_empty());
    }

    #[test]
    fn watch_from_a_retained_revision_replays_only_later_events() {
        let mut cache = WatchCache::new(vec![], 1, 16, 16);
        cache.apply(EventKind::Added, b"a".to_vec(), b"v1".to_vec(), 2);
        cache.apply(EventKind::Added, b"b".to_vec(), b"v2".to_vec(), 3);
        cache.apply(EventKind::Added, b"c".to_vec(), b"v3".to_vec(), 4);

        let (replay, _rx) = cache.watch_from(2).unwrap();
        assert_eq!(replay.len(), 2, "events after revision 2, i.e. revisions 3 and 4");
        assert_eq!(replay[0].revision, 3);
        assert_eq!(replay[1].revision, 4);
    }

    #[test]
    fn watch_from_older_than_the_retained_window_is_too_old() {
        let mut cache = WatchCache::new(vec![], 1, 16, /* history_limit */ 2);
        cache.apply(EventKind::Added, b"a".to_vec(), b"v1".to_vec(), 2);
        cache.apply(EventKind::Added, b"b".to_vec(), b"v2".to_vec(), 3);
        cache.apply(EventKind::Added, b"c".to_vec(), b"v3".to_vec(), 4);
        // history_limit=2 means only revisions 3 and 4 are still retained.
        let err = cache.watch_from(2).expect_err("revision 2 has been evicted from the retained window");
        assert!(matches!(err, Error::TooOld { requested: 2, oldest: 3 }));
    }

    #[test]
    fn bookmark_events_advance_revision_without_touching_any_key() {
        let mut cache = WatchCache::new(vec![(b"a".to_vec(), entry("v", 1))], 1, 16, 16);
        cache.apply(EventKind::Bookmark, Vec::new(), Vec::new(), 2);
        assert_eq!(cache.revision(), 2);
        let (items, _) = cache.list();
        assert_eq!(items.len(), 1, "a bookmark must not touch the item set");
    }

    #[tokio::test]
    async fn a_live_watcher_receives_events_applied_after_it_subscribes() {
        let mut cache = WatchCache::new(vec![], 1, 16, 16);
        let (_, mut rx) = cache.watch_from(0).unwrap();
        cache.apply(EventKind::Added, b"a".to_vec(), b"v".to_vec(), 2);
        let event = rx.recv().await.unwrap();
        assert_eq!(event.revision, 2);
        assert_eq!(event.key, b"a");
    }

    /// The whole reason `SharedCache` exists: a reader task calling `list()`
    /// and a writer task calling `apply()` concurrently, on two independent
    /// clones of the same underlying cache — genuinely concurrent (spawned
    /// tasks, not sequential calls on one binding), which is exactly what a
    /// bare `WatchCache` cannot support across `&self`/`&mut self` at the
    /// same time.
    #[tokio::test]
    async fn shared_cache_allows_concurrent_reads_and_a_writer() {
        let shared = SharedCache::new(WatchCache::new(vec![], 1, 16, 16));
        let writer = {
            let shared = shared.clone();
            tokio::spawn(async move {
                for rev in 2..=50 {
                    shared.apply(EventKind::Added, format!("k{rev}").into_bytes(), b"v".to_vec(), rev);
                    tokio::task::yield_now().await;
                }
            })
        };
        let reader = {
            let shared = shared.clone();
            tokio::spawn(async move {
                for _ in 0..50 {
                    let (_items, _rev) = shared.list();
                    tokio::task::yield_now().await;
                }
            })
        };
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            writer.await.unwrap();
            reader.await.unwrap();
        })
        .await
        .expect("concurrent read/write must not deadlock");
        assert_eq!(shared.revision(), 50);
        assert_eq!(shared.list().0.len(), 49);
    }

    #[test]
    fn shared_cache_replace_swaps_in_a_fresh_snapshot() {
        let shared = SharedCache::new(WatchCache::new(vec![(b"a".to_vec(), entry("old", 1))], 1, 16, 16));
        shared.replace(WatchCache::new(vec![(b"b".to_vec(), entry("new", 10))], 10, 16, 16));
        assert_eq!(shared.revision(), 10);
        let (items, _) = shared.list();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].0, b"b");
    }
}
