    let previous = state.remove(&crd_key).unwrap_or_default();
    let desired = crd.map(crd_cache_keys).unwrap_or_default();

    for (group, version, resource) in previous.difference(&desired) {
        registry.remove(group, version, resource);
    }
    for (group, version, resource) in desired.difference(&previous) {
        registry.spawn(storage.clone(), group, version, resource);
    }

