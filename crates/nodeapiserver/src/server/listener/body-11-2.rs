    loop {
        match events.recv().await {
            Ok(event) => match event.kind {
                crate::cacher::EventKind::Added | crate::cacher::EventKind::Modified => {
                    match rest::decrypt_and_decode(&storage, "apiextensions.k8s.io", "customresourcedefinitions", &event.key, &event.value) {
                        Ok(crd) => reconcile_crd_cache(&storage, &registry, &mut state, event.key, Some(&crd)),
                        Err(error) => warn!(error = ?error, "crd cache: failed to decode a changed CRD"),
                    }
                }
                crate::cacher::EventKind::Deleted => {
                    reconcile_crd_cache(&storage, &registry, &mut state, event.key, None);
                }
                crate::cacher::EventKind::Bookmark => {}
            },
            Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                warn!(skipped, "crd cache: event stream lagged; rebuilding dynamic cache registrations");
                let (entries, next_events) = crd_cache.snapshot_and_watch();
                let current_keys: HashSet<Vec<u8>> = entries.iter().map(|(key, _)| key.clone()).collect();
                let stale_keys: Vec<Vec<u8>> = state.keys().filter(|key| !current_keys.contains(*key)).cloned().collect();
                for key in stale_keys {
                    reconcile_crd_cache(&storage, &registry, &mut state, key, None);
                }
                for (key, entry) in entries {
                    match rest::decrypt_and_decode(&storage, "apiextensions.k8s.io", "customresourcedefinitions", &key, &entry.value) {
                        Ok(crd) => reconcile_crd_cache(&storage, &registry, &mut state, key, Some(&crd)),
                        Err(error) => warn!(error = ?error, "crd cache: failed to decode a CRD while rebuilding registrations"),
                    }
                }
                events = next_events;
            }
            Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
        }
    }
