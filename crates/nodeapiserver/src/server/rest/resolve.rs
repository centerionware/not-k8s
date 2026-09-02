/// The `Kind` this build serves at `(group, version, resource)`, or
/// `None` if this build doesn't know that resource at all. Pure and
/// unit-tested apart from [`get`]'s own network call.
pub fn resolve_kind(group: &str, version: &str, resource: &str) -> Option<&'static str> {
    codegen::api_resources_by_group_version()
        .get(&(group, version))?
        .iter()
        .find(|r| r.resource == resource)
        .map(|r| r.kind)
}

/// Resolves a parameter kind from a `ValidatingAdmissionPolicy`'s
/// `spec.paramKind`. Parameter kinds carry an API group and Kind but no
/// version or resource plural, so choose the most-preferred served version
/// from the static discovery table, then fall back to an Established CRD.
/// This is intentionally a read-only inverse of the normal resource lookup;
/// callers still use [`get`]` and [`list`]` for the actual parameter object.
pub async fn resolve_resource_for_kind(
    storage: &mut StorageClient,
    group: &str,
    kind: &str,
) -> Result<Option<(String, String, String, bool)>, Error> {
    let mut static_matches = codegen::api_resources::API_RESOURCES
        .iter()
        .filter(|resource| resource.group == group && resource.kind == kind)
        .collect::<Vec<_>>();
    static_matches.sort_by(|left, right| {
        super::version_compare::compare_kube_aware_versions(&right.version, &left.version)
    });
    if let Some(resource) = static_matches.into_iter().next() {
        return Ok(Some((
            resource.group.to_string(),
            resource.version.to_string(),
            resource.resource.to_string(),
            resource.namespaced,
        )));
    }

    let mut dynamic_matches =
        apiextensions::registry::discoverable_resources(list_stored_crds(storage).await?.iter())
            .into_iter()
            .filter(|resource| resource.group == group && resource.kind == kind)
            .collect::<Vec<_>>();
    dynamic_matches.sort_by(|left, right| {
        super::version_compare::compare_kube_aware_versions(&right.version, &left.version)
    });
    Ok(dynamic_matches.into_iter().next().map(|resource| {
        (
            resource.group,
            resource.version,
            resource.resource,
            resource.namespaced,
        )
    }))
}

/// What [`resolve_resource`] found `(group, version, resource)` to be —
/// either a built-in with a compiled proto schema (`resolve_kind`/
/// `schema_for_gvk`, unchanged from before Group K existed), or a
/// CRD-defined resource (`apiextensions::registry`), which has no
/// compiled schema at all: its body is stored/read as plain JSON, and
/// defaulting (when `open_api_schema` is present) walks that schema at
/// runtime instead of a compiled `FIELD_META` table
/// (`apiextensions::schema_defaults`).
struct ResolvedResource {
    kind: String,
    /// `Some(proto message name)` for a built-in; `None` for a CRD.
    schema: Option<&'static str>,
    open_api_schema: Option<Value>,
    /// Only ever meaningfully `true` for a CRD (`schema: None`) whose
    /// matched version declares `subresources.status` — always `true`
    /// for a static built-in, since this crate doesn't model per-type
    /// subresource declarations for built-ins at all yet (a real,
    /// separate, wider gap this field doesn't attempt to close — see
    /// `update_status`/`patch_status`'s own doc comment).
    has_status_subresource: bool,
}

/// The single place every real verb in this module decides what
/// `(group, version, resource)` actually is: the static, build-time
/// table first (no I/O, the overwhelmingly common case), falling back to
/// a live `LIST` of `CustomResourceDefinition`s only on a miss — Group
/// K's dynamic resource registry. `None` either way means a genuine
/// `UnknownResource` outcome to the caller, exactly as `resolve_kind`
/// alone used to mean.
///
/// **The CRD group itself is never recursed into** (`group.is_empty()`
/// covers the core group, which by definition has no CRDs in it
/// either): a request for `apiextensions.k8s.io/v1/customresourcedefinitions`
/// is always answered by the static table (Group A's codegen already
/// covers it — a `CustomResourceDefinition` is a real, compiled built-in
/// type, only the resources *it defines* are dynamic), so there's no risk
/// of this function ever listing CRDs to resolve a request for CRDs.
async fn resolve_resource(
    storage: &mut StorageClient,
    group: &str,
    version: &str,
    resource: &str,
) -> Result<Option<ResolvedResource>, Error> {
    if let Some(kind) = resolve_kind(group, version, resource) {
        return Ok(
            protobuf::schema_for_gvk(group, version, kind).map(|schema| ResolvedResource {
                kind: kind.to_string(),
                schema: Some(schema),
                open_api_schema: None,
                has_status_subresource: true,
            }),
        );
    }
    Ok(resolve_crd(storage, group, version, resource)
        .await?
        .map(|r| ResolvedResource {
            kind: r.kind,
            schema: None,
            open_api_schema: r.open_api_schema,
            has_status_subresource: r.has_status_subresource,
        }))
}

/// The dynamic (CRD-only) half of [`resolve_resource`] — skips the
/// static `resolve_kind` check entirely, so it's only ever correct to
/// call once a caller has already ruled that out itself.
/// `server::listener`'s own `WATCH` dispatch is the other real caller
/// besides [`resolve_resource`]: `watch` is served straight from an
/// already-registered `cacher::store::SharedCache` rather than through
/// any of this module's own generic verb functions, so it has no other
/// reason to reach into `server::rest` for a CRD-defined resource at
/// all — it needs only the Kind a matching `Established` CRD resolves
/// to, both to spawn a cache for it on first watch
/// (`cacher::registry::CacheRegistry::spawn`, callable at any time, not
/// just at boot) and to label the watch events it then streams.
async fn resolve_crd(
    storage: &mut StorageClient,
    group: &str,
    version: &str,
    resource: &str,
) -> Result<Option<apiextensions::registry::CrdResource>, Error> {
    if group.is_empty() || group == "apiextensions.k8s.io" {
        return Ok(None);
    }
    let crds = list_stored_crds(storage).await?;
    Ok(apiextensions::registry::resolve_in(
        crds.iter(),
        group,
        version,
        resource,
    ))
}

/// Public wrapper around [`resolve_crd`] for `server::listener`'s own
/// `WATCH` dispatch (the one caller outside this module that needs
/// Group K's dynamic registry directly — every other verb goes through
/// [`resolve_resource`] instead, which this module keeps private).
pub async fn resolve_dynamic_kind(
    storage: &mut StorageClient,
    group: &str,
    version: &str,
    resource: &str,
) -> Result<Option<String>, Error> {
    Ok(resolve_crd(storage, group, version, resource)
        .await?
        .map(|r| r.kind))
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

/// A raw `Range` over every stored `CustomResourceDefinition`, decoded —
/// deliberately *not* [`list`] itself: [`list`] calls [`resolve_resource`]
/// to find out what it's listing, and [`resolve_resource`]'s own CRD
/// fallback needs this same data, so calling back into `list` here would
/// be a real `async fn` recursion cycle (rejected outright by rustc,
/// `E0733` — infinitely-sized future, not merely a style objection) even
/// though it would never actually recurse more than once at runtime (the
/// CRD group is always resolved by the static table, never this
/// fallback). `customresourcedefinitions` is always cluster-scoped and
/// its own resource is never itself encrypted-at-rest-configurable in a
/// way this function needs to special-case — `decrypt_and_decode`
/// already handles "no transformer configured for this group/resource"
/// as a plain pass-through.
async fn list_stored_crds(storage: &mut StorageClient) -> Result<Vec<Value>, Error> {
    let prefix =
        keys::list_prefix("apiextensions.k8s.io", "customresourcedefinitions", None).into_bytes();
    let range_end = prefix_range_end(&prefix);
    let resp = storage
        .range(RangeRequest {
            key: prefix,
            range_end,
            ..Default::default()
        })
        .await?;
    resp.kvs
        .iter()
        .map(|kv| {
            decrypt_and_decode(
                storage,
                "apiextensions.k8s.io",
                "customresourcedefinitions",
                &kv.key,
                &kv.value,
            )
        })
        .collect()
}

/// Decodes a value exactly as stored in nodestore — the full `k8s\0`-
/// prefixed `runtime.Unknown` envelope `codec::protobuf::wrap_unknown`
/// produces — back into JSON. Pure and unit-tested with a real encoded
/// round trip, no network involved. Resolves the schema from the
/// envelope's own `apiVersion`/`kind` (what was actually written), not
/// from the caller's request path, so a decode is always faithful to
/// what's really stored even if the two ever disagreed.
pub fn decode_stored_object(bytes: &[u8]) -> Result<Value, protobuf::Error> {
    let (api_version, kind, object_bytes) = protobuf::unwrap_unknown(bytes)?;
    let (group, version) = split_api_version(&api_version);
    let mut object = match protobuf::schema_for_gvk(group, version, &kind) {
        Some(schema) => protobuf::decode_message(schema, &object_bytes),
        // Group K: no compiled schema for this Kind at all -- a CRD-
        // defined object, which `server::rest`'s write side always
        // stores as raw JSON in the envelope's `raw` field rather than
        // protobuf-encoding it (there's no compiled schema to encode
        // *with* either). A genuinely unknown, non-CRD Kind decodes to
        // the same `Json` error a malformed CRD body would -- this
        // function has no registry to tell the two apart, and both are
        // real "can't decode this" outcomes either way.
        None => Ok(serde_json::from_slice(&object_bytes).map_err(protobuf::Error::Json)?),
    }?;
    set_type_metadata(&mut object, &kind, &api_version);
    Ok(object)
}

/// Decodes a Kubernetes protobuf request envelope after resolving the
/// resource named by the URL. Built-in resources use their generated schema;
/// CRD objects use the envelope's raw JSON body because Kubernetes does not
/// generate a compiled protobuf schema for operator-defined kinds.
pub async fn decode_protobuf_request(
    storage: &mut StorageClient,
    group: &str,
    version: &str,
    resource: &str,
    bytes: &[u8],
) -> Result<Option<Value>, Error> {
    let Some(resolved) = resolve_resource(storage, group, version, resource).await? else {
        return Ok(None);
    };
    Ok(Some(decode_protobuf_object(&resolved, resource, bytes)?))
}

fn decode_protobuf_object(
    resolved: &ResolvedResource,
    resource: &str,
    bytes: &[u8],
) -> Result<Value, Error> {
    let (api_version, kind, object_bytes) = protobuf::unwrap_unknown(bytes)?;
    if kind != resolved.kind {
        return Err(Error::InvalidProtobufRequest(format!(
            "request kind {kind:?} does not match resource {resource:?}"
        )));
    }
    let (body_group, body_version) = split_api_version(&api_version);
    let mut object = match resolved
        .schema
        .or_else(|| protobuf::schema_for_gvk(body_group, body_version, &kind))
    {
        Some(schema) => protobuf::decode_message(schema, &object_bytes)?,
        None => serde_json::from_slice(&object_bytes).map_err(protobuf::Error::Json)?,
    };
    set_type_metadata(&mut object, &kind, &api_version);
    Ok(object)
}

/// Group C: the encrypted-aware counterpart to [`decode_stored_object`] —
/// decrypts `bytes` first when `storage` has a matching transformer for
/// `(group, resource)`, else decodes them as-is. Every real read call
/// site in this module (`get`, `list`, `update`'s own existing-object
/// read, `patch_prepare`, `update_status`, `patch_status`, `delete`, ...)
/// uses this instead of calling `decode_stored_object` directly, so
/// decryption happens in exactly one place regardless of which read path
/// is asking — the same centralization
/// `storage::encryption_config`'s own module doc comment named as the
/// reason this wiring was deferred until it could be done once, for
/// everything, rather than gap-by-gap.
///
/// `key` is the object's own real etcd key — required as AES-GCM's
/// authenticated data (`storage::encryption::Transformer`'s own doc
/// comment: "so a ciphertext can't be copied to a different key and
/// still decrypt"), matching real upstream's own
/// `dataCtx.AuthenticatedData()` convention exactly. The real upstream
/// `stale` flag `transform_from_storage` returns (real upstream's own
/// signal that a value was encrypted under a non-primary key — a
/// migration-in-progress marker meaning "rewrite this with the current
/// primary key next time it's written") is intentionally discarded here:
/// this build has nowhere to act on it yet (no background re-encryption
/// sweep), a named, narrower gap than the wiring itself, not silently
/// dropped without comment.
pub(crate) fn decrypt_and_decode(
    storage: &StorageClient,
    group: &str,
    resource: &str,
    key: &[u8],
    bytes: &[u8],
) -> Result<Value, Error> {
    match storage.transformers_for(group, resource) {
        Some(transformers) => {
            let (plaintext, _stale) = transformers.transform_from_storage(bytes, key)?;
            Ok(decode_stored_object(&plaintext)?)
        }
        None => Ok(decode_stored_object(bytes)?),
    }
}

/// The write-side counterpart to [`decrypt_and_decode`]: encrypts `bytes`
/// (a real `wrap_unknown` envelope) when `storage` has a matching
/// transformer for `(group, resource)`, else returns it unchanged. Both
/// real `PutRequest` construction sites in this crate (`create`,
/// `persist_update`, the latter shared by `update`/`patch`/
/// `update_status`/`patch_status`) call this immediately before building
/// the request — nothing this crate writes to nodestore ever bypasses
/// this when encryption is actually configured for its resource.
pub(crate) fn encrypt_for_storage(
    storage: &StorageClient,
    group: &str,
    resource: &str,
    key: &[u8],
    bytes: &[u8],
) -> Result<Vec<u8>, Error> {
    match storage.transformers_for(group, resource) {
        Some(transformers) => Ok(transformers.transform_to_storage(bytes, key)?),
        None => Ok(bytes.to_vec()),
    }
}

/// `""` -> `("", "")` (never real — `apiVersion` is empty only for a
/// malformed/never-written envelope), `"v1"` -> `("", "v1")` (the core
/// group has no group segment in `apiVersion`), `"apps/v1"` ->
/// `("apps", "v1")`.
fn split_api_version(api_version: &str) -> (&str, &str) {
    match api_version.split_once('/') {
        Some((group, version)) => (group, version),
        None => ("", api_version),
    }
}
