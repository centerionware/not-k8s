//! A registry of per-resource watch caches, each kept live by its own
//! `cacher::driver::reflect()` background task.
//!
//! # What this is, and deliberately isn't yet
//!
//! This module can start one reflector for one resource and hand back a
//! [`SharedCache`] to read from. The listener uses that primitive for every
//! built-in resource at startup; CRD-defined resources are registered lazily
//! once their live discovery entry exists.
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
use tokio::sync::watch;

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
    caches: Arc<RwLock<HashMap<ResourceKey, Registration>>>,
}

struct Registration {
    cache: SharedCache,
    stop: watch::Sender<bool>,
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
        self.caches
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&key)
            .map(|registration| registration.cache.clone())
    }

    /// Starts a background `reflect()` loop for one resource and
    /// registers the [`SharedCache`] it keeps live, replacing any
    /// previous registration for the same key. Replacing a registration
    /// cancels the old reflector before its cache is forgotten, so a CRD
    /// delete/recreate or an explicit re-registration cannot leak a second
    /// nodestore watch task.
    /// Returns the cache immediately; it starts empty and populates once
    /// the first `LIST` completes — a reader that consults it before then
    /// just sees a not-yet-synced cache, the same window a real
    /// `client-go` informer's own `HasSynced()` describes.
    pub fn spawn(&self, mut storage: StorageClient, group: &str, version: &str, resource: &str) -> SharedCache {
        let key_prefix = keys::list_prefix(group, resource, None).into_bytes();
        let cache = SharedCache::new(WatchCache::new(Vec::new(), 0, DEFAULT_EVENT_BUFFER, DEFAULT_HISTORY_LIMIT));
        let (stop, stop_receiver) = watch::channel(false);

        let key = (group.to_string(), version.to_string(), resource.to_string());
        let previous = self
            .caches
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(key, Registration { cache: cache.clone(), stop });
        if let Some(previous) = previous {
            let _ = previous.stop.send(true);
        }

        let reflect_cache = cache.clone();
        tokio::spawn(async move {
            reflect(
                &mut storage,
                &key_prefix,
                &reflect_cache,
                DEFAULT_EVENT_BUFFER,
                DEFAULT_HISTORY_LIMIT,
                DEFAULT_RECONNECT_BACKOFF,
                DEFAULT_BOOKMARK_INTERVAL,
                stop_receiver,
            )
            .await;
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
        registry.caches.write().unwrap().insert(
            ("".to_string(), "v1".to_string(), "pods".to_string()),
            Registration {
                cache: SharedCache::new(WatchCache::new(Vec::new(), 0, 8, 8)),
                stop: watch::channel(false).0,
            },
        );
        assert!(registry.get("", "v1", "nodes").is_none());
        assert!(registry.get("", "v1", "pods").is_some());
    }
}
