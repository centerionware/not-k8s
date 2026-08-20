//! A registry of per-resource watch caches, each kept live by its own
//! `cacher::driver::reflect()` background task — the piece named "not yet
//! started" repeatedly across this crate's own doc comments (nothing in
//! `lib.rs::run()` calls `reflect()` at all today).
//!
//! # What this is, and deliberately isn't yet
//!
//! This module can start one reflector for one resource and hand back a
//! [`SharedCache`] to read from — the primitive. It does **not** yet
//! enumerate "every resource this build knows about" and start one for
//! each at boot: spawning on the order of 90 concurrent, long-running
//! reconnect loops against nodestore at process startup is a real
//! resource/ordering decision this crate hasn't made yet (how many at
//! once, in what order, whether to wait for any to sync before serving
//! traffic), not an oversight. That boot-time integration for *every*
//! resource is the remaining, not-yet-started follow-up work — the read
//! side is no longer blocked on it: `server::rest::get`/`list` both
//! already consult whatever cache a caller hands them (`rest`'s own doc
//! comment), and `server::listener::run` now calls `spawn` for a
//! deliberately bounded, reasoned list of resources
//! (`BOOT_CACHED_RESOURCES`), not just the original one-resource
//! (`namespaces`) proof of concept.
//!
//! Cache scope matches real kube-apiserver's own: one cache per
//! `(group, version, resource)`, covering every namespace at once (not
//! one cache per namespace) — `storage::keys::list_prefix`'s own
//! `namespace: None` form is exactly this whole-resource prefix.

use crate::cacher::driver::reflect;
use crate::cacher::store::{SharedCache, WatchCache};
use crate::storage::client::StorageClient;
use crate::storage::keys;
use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::Duration;

/// `(group, version, resource)` — the same triple `server::rest`'s own
/// functions key resources by.
pub type ResourceKey = (String, String, String);

/// Real `client-go` `SharedInformerFactory` defaults use similar orders
/// of magnitude for these; picked for the same reason — large enough
/// that a burst of events (a controller doing a bulk create/delete) does
/// not immediately force a relist, small enough that one idle resource's
/// cache isn't holding an unbounded amount of history.
const DEFAULT_EVENT_BUFFER: usize = 1024;
const DEFAULT_HISTORY_LIMIT: usize = 1024;
const DEFAULT_RECONNECT_BACKOFF: Duration = Duration::from_secs(1);
const DEFAULT_BOOKMARK_INTERVAL: Duration = Duration::from_secs(60);

#[derive(Clone, Default)]
pub struct CacheRegistry {
    caches: Arc<RwLock<HashMap<ResourceKey, SharedCache>>>,
}

impl CacheRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// The cache for `(group, version, resource)`, if a reflector has
    /// been [`spawn`](Self::spawn)ed for it — `None` otherwise, not an
    /// empty cache; a caller needs to tell "nothing registered" apart
    /// from "registered but genuinely empty" (a resource with zero live
    /// objects is a real, valid state).
    pub fn get(&self, group: &str, version: &str, resource: &str) -> Option<SharedCache> {
        let key = (group.to_string(), version.to_string(), resource.to_string());
        self.caches.read().unwrap_or_else(std::sync::PoisonError::into_inner).get(&key).cloned()
    }

    /// Starts a background `reflect()` loop for one resource and
    /// registers the [`SharedCache`] it keeps live, replacing any
    /// previous registration for the same key (the old reflector, if any,
    /// keeps running against its own now-orphaned cache until the process
    /// exits — restarting a registration cleanly is separate, not-yet-needed
    /// work, since nothing calls this more than once per resource yet).
    /// Returns the cache immediately; it starts empty and populates once
    /// the first `LIST` completes — a reader that consults it before then
    /// just sees a not-yet-synced cache, the same window a real
    /// `client-go` informer's own `HasSynced()` describes.
    pub fn spawn(&self, mut storage: StorageClient, group: &str, version: &str, resource: &str) -> SharedCache {
        let key_prefix = keys::list_prefix(group, resource, None).into_bytes();
        let cache = SharedCache::new(WatchCache::new(Vec::new(), 0, DEFAULT_EVENT_BUFFER, DEFAULT_HISTORY_LIMIT));

        let key = (group.to_string(), version.to_string(), resource.to_string());
        self.caches.write().unwrap_or_else(std::sync::PoisonError::into_inner).insert(key, cache.clone());

        let reflect_cache = cache.clone();
        tokio::spawn(async move {
            reflect(&mut storage, &key_prefix, &reflect_cache, DEFAULT_EVENT_BUFFER, DEFAULT_HISTORY_LIMIT, DEFAULT_RECONNECT_BACKOFF, DEFAULT_BOOKMARK_INTERVAL).await;
        });

        cache
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fresh_registry_has_nothing_registered() {
        let registry = CacheRegistry::new();
        assert!(registry.get("", "v1", "pods").is_none());
    }

    #[test]
    fn get_is_none_for_a_resource_nothing_has_spawned() {
        let registry = CacheRegistry::new();
        // Registering "pods" must not make "nodes" appear too.
        registry.caches.write().unwrap().insert(("".to_string(), "v1".to_string(), "pods".to_string()), SharedCache::new(WatchCache::new(Vec::new(), 0, 8, 8)));
        assert!(registry.get("", "v1", "nodes").is_none());
        assert!(registry.get("", "v1", "pods").is_some());
    }
}
