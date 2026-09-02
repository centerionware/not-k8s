
/// Public wrapper around [`resolve_crd`] for `server::listener`'s own
/// `WATCH` dispatch (the one caller outside this module that needs
/// Group K's dynamic registry directly — every other verb goes through
/// [`resolve_resource`] instead, which this module keeps private).
pub async fn resolve_dynamic_kind(storage: &mut StorageClient, group: &str, version: &str, resource: &str) -> Result<Option<String>, Error> {
    Ok(resolve_dynamic_resource(storage, group, version, resource).await?.map(|r| r.kind))
}

/// Public dynamic-registry lookup for callers that need more than the
/// resolved Kind. In particular, the watch path needs the CRD's conversion
/// webhook configuration while it formats events from the storage-version
/// cache for a client's requested served version.
pub async fn resolve_dynamic_resource(
    storage: &mut StorageClient,
    group: &str,
    version: &str,
    resource: &str,
) -> Result<Option<apiextensions::registry::CrdResource>, Error> {
    resolve_crd(storage, group, version, resource).await
}

/// Every stored `CustomResourceDefinition`, decoded — `server::listener`'s
/// own discovery-merge call site is the other real caller outside this
/// module that needs the raw documents (not just one resolved GVR): it
/// merges every served, `Established` CRD's own resources into
/// `/apis`/`/apis/{group}`/`/apis/{group}/{version}` discovery output
/// (`apiextensions::registry::discoverable_resources` does the actual
/// filtering/shaping). Public so that call site doesn't need its own
/// copy of the raw-`Range`-plus-decode this module already has.
pub async fn list_all_crds(storage: &mut StorageClient) -> Result<Vec<Value>, Error> {
    list_stored_crds(storage).await
}
