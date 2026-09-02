    crd_cache.wait_until_synced().await;
    let (entries, mut events) = crd_cache.snapshot_and_watch();
    let mut state = DynamicCacheState::new();

    for (key, entry) in entries {
        match rest::decrypt_and_decode(&storage, "apiextensions.k8s.io", "customresourcedefinitions", &key, &entry.value) {
            Ok(crd) => reconcile_crd_cache(&storage, &registry, &mut state, key, Some(&crd)),
            Err(error) => warn!(error = ?error, "crd cache: failed to decode an initial CRD"),
        }
    }

