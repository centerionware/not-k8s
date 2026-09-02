
fn crd_cache_keys(crd: &serde_json::Value) -> HashSet<crate::cacher::registry::ResourceKey> {
    crate::apiextensions::registry::discoverable_resources(std::iter::once(crd))
        .into_iter()
        .map(|resource| (resource.group, resource.version, resource.resource))
        .collect()
}
