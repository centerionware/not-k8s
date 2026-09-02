
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("nodestore request failed: {0}")]
    Storage(#[from] StorageError),
    #[error("decoding the stored object failed: {0}")]
    Decode(#[from] protobuf::Error),
    #[error("invalid selector: {0}")]
    Selector(#[from] ParseError),
    #[error("encryption transform failed: {0}")]
    Encryption(#[from] crate::storage::encryption::Error),
    #[error("invalid protobuf request: {0}")]
    InvalidProtobufRequest(String),
    #[error("the requested resource is not served")]
    UnknownResource,
}

#[derive(Debug, PartialEq)]
pub enum GetOutcome {
    /// The decoded object, ready to serialize.
    Found(Value),
    /// This build has no such `(group, version, resource)` at all — same
    /// "real 404, not a silent fallthrough" reasoning
    /// `server::discovery`'s own `NotFound` case already established.
    UnknownResource,
    /// The resource is known, but no object exists at that key.
    ObjectNotFound,
}

/// The `Kind` this build serves at `(group, version, resource)`, or
/// `None` if this build doesn't know that resource at all. Pure and
/// unit-tested apart from [`get`]'s own network call.
pub fn resolve_kind(group: &str, version: &str, resource: &str) -> Option<&'static str> {
    codegen::api_resources_by_group_version().get(&(group, version))?.iter().find(|r| r.resource == resource).map(|r| r.kind)
}

/// Resolves a parameter kind from a `ValidatingAdmissionPolicy`'s
/// `spec.paramKind`. Parameter kinds carry an API group and Kind but no
/// version or resource plural, so choose the most-preferred served version
/// from the static discovery table, then fall back to an Established CRD.
/// This is intentionally a read-only inverse of the normal resource lookup;
/// callers still use [`get`]` and [`list`]` for the actual parameter object.
pub async fn resolve_resource_for_kind(storage: &mut StorageClient, group: &str, kind: &str) -> Result<Option<(String, String, String, bool)>, Error> {
    let mut static_matches = codegen::api_resources::API_RESOURCES
        .iter()
        .filter(|resource| resource.group == group && resource.kind == kind)
        .collect::<Vec<_>>();
    static_matches.sort_by(|left, right| super::version_compare::compare_kube_aware_versions(&right.version, &left.version));
    if let Some(resource) = static_matches.into_iter().next() {
        return Ok(Some((resource.group.to_string(), resource.version.to_string(), resource.resource.to_string(), resource.namespaced)));
    }

    let mut dynamic_matches = apiextensions::registry::discoverable_resources(list_stored_crds(storage).await?.iter())
        .into_iter()
        .filter(|resource| resource.group == group && resource.kind == kind)
        .collect::<Vec<_>>();
    dynamic_matches.sort_by(|left, right| super::version_compare::compare_kube_aware_versions(&right.version, &left.version));
    Ok(dynamic_matches.into_iter().next().map(|resource| (resource.group, resource.version, resource.resource, resource.namespaced)))
}

/// Resolve the served resource's namespacedness for admission matching. The
/// static discovery table handles built-ins without I/O; a CRD lookup uses
/// the same established definitions as ordinary REST resolution.
pub async fn resource_is_namespaced(storage: &mut StorageClient, group: &str, version: &str, resource: &str) -> Result<Option<bool>, Error> {
    if let Some(found) = codegen::api_resources_by_group_version()
        .get(&(group, version))
        .and_then(|resources| resources.iter().find(|candidate| candidate.resource == resource))
    {
        return Ok(Some(found.namespaced));
    }
    let crds = list_stored_crds(storage).await?;
    Ok(apiextensions::registry::discoverable_resources(crds.iter())
        .into_iter()
        .find(|candidate| candidate.group == group && candidate.version == version && candidate.resource == resource)
        .map(|candidate| candidate.namespaced))
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
    /// The CRD storage version's schema, when this is a dynamic resource.
    /// Requests are validated against their served version before conversion;
    /// converted objects must also satisfy this schema before persistence.
    storage_open_api_schema: Option<Value>,
    /// Only ever meaningfully `true` for a CRD (`schema: None`) whose
    /// matched version declares `subresources.status` — always `true`
    /// for a static built-in, since this crate doesn't model per-type
    /// subresource declarations for built-ins at all yet (a real,
    /// separate, wider gap this field doesn't attempt to close — see
    /// `update_status`/`patch_status`'s own doc comment).
    has_status_subresource: bool,
    conversion_webhook: Option<apiextensions::registry::ConversionWebhook>,
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
async fn resolve_resource(storage: &mut StorageClient, group: &str, version: &str, resource: &str) -> Result<Option<ResolvedResource>, Error> {
    if let Some(kind) = resolve_kind(group, version, resource) {
        return Ok(protobuf::schema_for_gvk(group, version, kind).map(|schema| ResolvedResource { kind: kind.to_string(), schema: Some(schema), open_api_schema: None, storage_open_api_schema: None, has_status_subresource: true, conversion_webhook: None }));
    }
    Ok(resolve_crd(storage, group, version, resource)
        .await?
        .map(|r| ResolvedResource { kind: r.kind, schema: None, open_api_schema: r.open_api_schema, storage_open_api_schema: r.storage_open_api_schema, has_status_subresource: r.has_status_subresource, conversion_webhook: r.conversion_webhook }))
}

/// Resolve the OpenAPI schema used to declare CEL mutation object aliases.
/// Built-in schemas come from the same vendored document advertised by
/// `/openapi/v3`; CRD schemas come from their established version directly.
/// Built-in references are expanded here so the CEL environment can register
/// names such as `Object.spec.containers` without duplicating schema lookup
/// rules in the admission layer.
pub async fn mutation_openapi_schema(storage: &mut StorageClient, group: &str, version: &str, resource: &str) -> Result<Option<Value>, Error> {
    let Some(resolved) = resolve_resource(storage, group, version, resource).await? else {
        return Ok(None);
    };
    if let Some(schema) = resolved.open_api_schema {
        return Ok(Some(schema));
    }
    let Some(schema_name) = resolved.schema else {
        return Ok(None);
    };
    let path = if group.is_empty() {
        format!("api/{version}")
    } else {
        format!("apis/{group}/{version}")
    };
    let Some(document) = codegen::openapi_v3_document(&path) else {
        return Ok(None);
    };
    let Some(schemas) = document.pointer("/components/schemas").and_then(Value::as_object) else {
        return Ok(None);
    };
    let Some(root) = schemas.get(schema_name) else {
        return Ok(None);
    };
    Ok(Some(expand_openapi_refs(root, schemas, &mut BTreeSet::new())))
}

fn expand_openapi_refs(value: &Value, schemas: &Map<String, Value>, active: &mut BTreeSet<String>) -> Value {
    match value {
        Value::Array(values) => Value::Array(values.iter().map(|value| expand_openapi_refs(value, schemas, active)).collect()),
        Value::Object(object) => {
            if let Some(reference) = object.get("$ref").and_then(Value::as_str).and_then(|reference| reference.strip_prefix("#/components/schemas/")) {
                if let Some(target) = schemas.get(reference) {
                    if active.insert(reference.to_string()) {
                        let mut expanded = expand_openapi_refs(target, schemas, active);
                        active.remove(reference);
                        if let Value::Object(expanded_object) = &mut expanded {
                            for (key, value) in object {
                                if key != "$ref" {
                                    expanded_object.insert(key.clone(), expand_openapi_refs(value, schemas, active));
                                }
                            }
                        }
                        return expanded;
                    }
                    // Recursive OpenAPI types cannot be represented by a
                    // finite CEL struct tree. Keep the recursive edge
                    // dynamic while retaining the containing object fields.
                    return json!({"type": "object"});
                }
            }
            Value::Object(object.iter().map(|(key, value)| (key.clone(), expand_openapi_refs(value, schemas, active))).collect())
        }
        _ => value.clone(),
    }
}

async fn convert_to_storage_version(
    storage: &mut StorageClient,
    group: &str,
    version: &str,
    conversion_webhook: Option<&apiextensions::registry::ConversionWebhook>,
    object: Value,
) -> Result<Value, Error> {
    let Some(conversion_webhook) = conversion_webhook else {
        return Ok(object);
    };
    if conversion_webhook.storage_version == version {
        return Ok(object);
    }
    let mut objects = apiextensions::conversion::convert(storage, group, conversion_webhook, &conversion_webhook.storage_version, vec![object]).await.map_err(|error| Error::InvalidProtobufRequest(error.to_string()))?;
    objects.pop().ok_or_else(|| Error::InvalidProtobufRequest("conversion webhook returned no object".to_string()))
}

async fn convert_to_requested_version(
    storage: &mut StorageClient,
    group: &str,
    version: &str,
    kind: &str,
    conversion_webhook: Option<&apiextensions::registry::ConversionWebhook>,
    object: Value,
) -> Result<Value, Error> {
    let object = if let Some(conversion_webhook) = conversion_webhook {
        let source_version = object
            .get("apiVersion")
            .and_then(Value::as_str)
            .map(|api_version| api_version.rsplit_once('/').map_or(api_version, |(_, version)| version));
        if source_version != Some(version) {
            let mut objects = apiextensions::conversion::convert(storage, group, conversion_webhook, version, vec![object]).await.map_err(|error| Error::InvalidProtobufRequest(error.to_string()))?;
            objects.pop().ok_or_else(|| Error::InvalidProtobufRequest("conversion webhook returned no object".to_string()))?
        } else {
            object
        }
    } else {
        object
    };
    Ok(crate::scheme::conversion::to_version(group, version, kind, object))
}

/// Validate and prune an object after a conversion webhook has produced the
/// representation that will be written. The request version's schema is not
/// sufficient here: a webhook may return an object that passed validation
/// before conversion but does not satisfy the storage version's schema.
fn revalidate_storage_object(schema: Option<&Value>, object: Value) -> Result<Value, Vec<String>> {
    let Some(schema) = schema else {
        return Ok(object);
    };
    let object = apiextensions::schema_pruning::prune(schema, &object);
    let mut violations = apiextensions::schema_validation::validate_required(schema, &object)
        .into_iter()
        .map(|violation| format!("{}: Required value", violation.path))
        .collect::<Vec<_>>();
    violations.extend(
        apiextensions::schema_validation::validate_types(schema, &object)
            .into_iter()
            .map(|violation| format!("{}: expected type {}, got {}", violation.path, violation.expected, violation.actual_kind)),
    );
    violations.extend(apiextensions::schema_validation::validate_constraints(schema, &object));
    if violations.is_empty() { Ok(object) } else { Err(violations) }
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
async fn resolve_crd(storage: &mut StorageClient, group: &str, version: &str, resource: &str) -> Result<Option<apiextensions::registry::CrdResource>, Error> {
    if group.is_empty() || group == "apiextensions.k8s.io" {
        return Ok(None);
    }
    let crds = list_stored_crds(storage).await?;
    Ok(apiextensions::registry::resolve_in(crds.iter(), group, version, resource))
}

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
    let prefix = keys::list_prefix("apiextensions.k8s.io", "customresourcedefinitions", None).into_bytes();
    let range_end = prefix_range_end(&prefix);
    let resp = storage.range(RangeRequest { key: prefix, range_end, ..Default::default() }).await?;
    let mut objects = Vec::with_capacity(resp.kvs.len());
    for kv in resp.kvs {
        objects.push(decrypt_and_decode_with_rotation(
            storage,
            "apiextensions.k8s.io",
            "customresourcedefinitions",
            &kv.key,
            &kv.value,
            kv.mod_revision,
        ).await?);
    }
    Ok(objects)
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

fn decode_protobuf_object(resolved: &ResolvedResource, resource: &str, bytes: &[u8]) -> Result<Value, Error> {
    let (api_version, kind, object_bytes) = protobuf::unwrap_unknown(bytes)?;
    if kind != resolved.kind {
        return Err(Error::InvalidProtobufRequest(format!("request kind {kind:?} does not match resource {resource:?}")));
    }
    let (body_group, body_version) = split_api_version(&api_version);
    let mut object = match resolved.schema.or_else(|| protobuf::schema_for_gvk(body_group, body_version, &kind)) {
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
/// The plain helper is retained for synchronous callers such as watch-event
/// formatting. Async REST reads use [`decrypt_and_decode_with_rotation`],
/// which honors the transformer's stale-key signal after decoding.
pub(crate) fn decrypt_and_decode(storage: &StorageClient, group: &str, resource: &str, key: &[u8], bytes: &[u8]) -> Result<Value, Error> {
    match storage.transformers_for(group, resource) {
        Some(transformers) => {
            let (plaintext, _stale) = transformers.transform_from_storage(bytes, key)?;
            Ok(decode_stored_object(&plaintext)?)
        }
        None => Ok(decode_stored_object(bytes)?),
    }
}

/// The read path with upstream's key-rotation behavior. A value encrypted
/// with any non-primary provider/key is returned normally, then rewritten
/// with the first configured transformer only if the optimistic-concurrency
/// check still sees the revision that was read. Rotation is bookkeeping: a
/// failed or raced rewrite must never turn a successful read into an API
/// error, and the next read can safely try again.
pub(crate) async fn decrypt_and_decode_with_rotation(
    storage: &mut StorageClient,
    group: &str,
    resource: &str,
    key: &[u8],
    bytes: &[u8],
    revision: i64,
) -> Result<Value, Error> {
    let Some(transformers) = storage.transformers_for(group, resource) else {
        return Ok(decode_stored_object(bytes)?);
    };
    let (plaintext, stale) = transformers.transform_from_storage(bytes, key)?;
    let object = decode_stored_object(&plaintext)?;

    if stale && revision > 0 {
        let rotated = match encrypt_for_storage(storage, group, resource, key, &plaintext) {
            Ok(rotated) => rotated,
            Err(error) => {
                tracing::warn!(group, resource, revision, error = ?error, "storage: stale-key rewrite could not encrypt the value; returning the decrypted value");
                return Ok(object);
            }
        };
        let compare = pb::Compare {
            key: key.to_vec(),
            result: pb::compare::CompareResult::Equal as i32,
            target: pb::compare::CompareTarget::Mod as i32,
            target_union: Some(pb::compare::TargetUnion::ModRevision(revision)),
            range_end: Vec::new(),
        };
        let txn = pb::TxnRequest {
            compare: vec![compare],
            success: vec![pb::RequestOp {
                request: Some(pb::request_op::Request::RequestPut(pb::PutRequest {
                    key: key.to_vec(),
                    value: rotated,
                    ..Default::default()
                })),
            }],
            failure: Vec::new(),
        };
        match storage.txn(txn).await {
            Ok(response) if response.succeeded => {
                tracing::debug!(group, resource, revision, "storage: re-encrypted a value with the primary key");
            }
            Ok(_) => {
                tracing::debug!(group, resource, revision, "storage: skipped stale-key rewrite after a concurrent update");
            }
            Err(error) => {
                tracing::warn!(group, resource, revision, error = ?error, "storage: stale-key rewrite failed; returning the decrypted value");
            }
        }
    }

    Ok(object)
}

/// The write-side counterpart to [`decrypt_and_decode`]: encrypts `bytes`
/// (a real `wrap_unknown` envelope) when `storage` has a matching
/// transformer for `(group, resource)`, else returns it unchanged. Both
/// real `PutRequest` construction sites in this crate (`create`,
/// `persist_update`, the latter shared by `update`/`patch`/
/// `update_status`/`patch_status`) call this immediately before building
/// the request — nothing this crate writes to nodestore ever bypasses
/// this when encryption is actually configured for its resource.
pub(crate) fn encrypt_for_storage(storage: &StorageClient, group: &str, resource: &str, key: &[u8], bytes: &[u8]) -> Result<Vec<u8>, Error> {
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

/// Fetches and decodes a single object. `namespace` is `None` for a
/// cluster-scoped resource (matches `storage::keys::object_key`'s own
/// convention) — the caller (`server::listener`) is responsible for
/// turning `path::RequestInfo`'s always-`String` `namespace` field into
/// this `Option` (empty string -> `None`).
///
/// `cache`, if given, is consulted first (`cacher::store::SharedCache::get`,
/// Group D) — a hit skips the `Range` round trip to nodestore entirely.
/// A **miss always falls through to nodestore**, unconditionally, rather
/// than trusting the cache to say "not found": a `cache: Some(_)` that
/// hasn't finished its first `LIST` yet (or isn't registered for this
/// exact resource at all, if a caller ever passed the wrong one) is
/// indistinguishable from "genuinely empty" using only what `SharedCache`
/// exposes today, so treating a miss as authoritative would risk a false
/// `404` during that window. This makes cache consultation a pure
/// latency optimization on the hit path, never a correctness risk on the
/// miss path — real upstream's own watch cache takes the same
/// "consistent read falls through" posture for exactly this reason.
/// `None` behaves exactly as before this parameter existed; callers outside
/// the listener's cache path can still pass `None`.
pub async fn get(storage: &mut StorageClient, cache: Option<&crate::cacher::store::SharedCache>, group: &str, version: &str, resource: &str, namespace: Option<&str>, name: &str) -> Result<GetOutcome, Error> {
    get_at_revision(storage, cache, group, version, resource, namespace, name, 0).await
}

/// [`get`] with an optional etcd MVCC snapshot revision. A non-positive
/// revision retains the normal current-state behavior.
pub async fn get_at_revision(storage: &mut StorageClient, cache: Option<&crate::cacher::store::SharedCache>, group: &str, version: &str, resource: &str, namespace: Option<&str>, name: &str, resource_version: i64) -> Result<GetOutcome, Error> {
    let Some(resolved) = resolve_resource(storage, group, version, resource).await? else {
        return Ok(GetOutcome::UnknownResource);
    };
    let kind = resolved.kind.clone();
    let key = keys::object_key(group, resource, namespace, name);

    if resource_version <= 0 {
        if let Some(cache) = cache {
            if let Some(entry) = cache.get(key.as_bytes()) {
                let mut object = decrypt_and_decode_with_rotation(storage, group, resource, key.as_bytes(), &entry.value, entry.mod_revision).await?;
                set_metadata_field(&mut object, "resourceVersion", Value::String(entry.mod_revision.to_string()));
                let object = convert_to_requested_version(storage, group, version, &kind, resolved.conversion_webhook.as_ref(), object).await?;
                return Ok(GetOutcome::Found(object));
            }
        }
    }

    let resp = storage.range(RangeRequest { key: key.into_bytes(), revision: resource_version.max(0), ..Default::default() }).await?;
    let Some(kv) = resp.kvs.into_iter().next() else {
        return Ok(GetOutcome::ObjectNotFound);
    };
    let mut object = decrypt_and_decode_with_rotation(storage, group, resource, &kv.key, &kv.value, kv.mod_revision).await?;
    // Real, load-bearing fix, found live (`tests/apiservice_roundtrip.rs`'s
    // own get-then-update round trip): `resourceVersion` is never
    // actually *persisted* into the stored object bytes (`create`/
    // `persist_update` both stamp it onto their own return value only
    // *after* the write that produced it — the revision doesn't exist
    // yet while the bytes being written are still being built, so there
    // is nothing earlier to persist it into either) — matching real
    // upstream's own posture, where `resourceVersion` is always etcd's
    // own `mod_revision` read back at serve time, never object content.
    // A plain read has to do the same real-time stamping every write
    // path already does, from this exact `Range`'s own `kv.mod_revision`
    // — every prior write-then-read-back test in this crate happened to
    // use a `create`/`update` call's own return value directly, which
    // already carried a real `resourceVersion`, so nothing exercised a
    // genuine `GET` followed by an `UPDATE` until this one did.
    set_metadata_field(&mut object, "resourceVersion", Value::String(kv.mod_revision.to_string()));
    let object = convert_to_requested_version(storage, group, version, &kind, resolved.conversion_webhook.as_ref(), object).await?;
    Ok(GetOutcome::Found(object))
}

#[derive(Debug, PartialEq)]
pub enum ListOutcome {
    /// The real `<Kind>List` document, ready to serialize.
    Found(Value),
    UnknownResource,
    /// The submitted `continue` token didn't decode — not valid base64,
    /// no `0x00` key/revision separator, or a non-numeric revision.
    /// Real upstream's own `errors.NewBadRequest("continue token is not
    /// valid")` shape, not a `500`.
    InvalidContinueToken,
}

/// The real `<Kind>List` `kind` value for a resource this build serves —
/// standard Kubernetes convention, verified against real vendored data:
/// every List type in the vendored OpenAPI specs is named exactly
/// `<Kind>List` (`PodList`, `DeploymentList`, ...), never a separate
/// hand-assigned name.
fn list_kind(kind: &str) -> String {
    format!("{kind}List")
}

/// Lists every object of a resource — the whole resource, or scoped to
/// one namespace (`namespace: None` for a cluster-scoped resource, same
/// convention as [`get`]). Items are decoded and filtered
/// (`cacher::selector::object_matches`) the same way regardless of source.
/// Items are returned in whatever order the source hands them back in
/// (key order, for both a real `Range` and the cache's own `BTreeMap`) —
/// real upstream doesn't guarantee list ordering either.
/// `label_selector`/`field_selector` are the raw query-string values
/// `path::RequestInfo` already captures for `list` (empty means "no
/// constraint from that half," matching upstream's own `Everything()`
/// selector semantics). `limit`/`continue_token` are real pagination —
/// `limit <= 0` means "no limit" (matching real upstream's own `0`
/// convention), and a non-empty `continue_token` resumes an earlier
/// paginated listing (real upstream's own contract: opaque to the
/// client, only ever handed back verbatim from a prior page's own
/// `metadata.continue`). A paginated request always bypasses the watch
/// cache (see below) and reads directly from nodestore, since real
/// pagination is a genuine ordered range-scan-with-resume-point, which
/// the cache's own unordered in-memory store doesn't support. Real
/// upstream's own documented caveat applies here too: label/field
/// selector filtering happens *after* the limited range fetch, so a
/// page can come back with fewer than `limit` items (even zero) despite
/// more matching items existing on later pages.
///
/// `cache`, if given, is consulted first — but only once
/// [`crate::cacher::store::SharedCache::has_synced`] is true. Unlike
/// [`get`]'s "a miss always falls through" trick, `list` can't use that
/// same safety net: a cache that hasn't finished its first `LIST` yet
/// would report zero items, and zero items is itself a fully valid `LIST`
/// answer (a real `200`, not a `404`) — there is no way to tell "empty
/// because unsynced" from "empty because genuinely empty" after the fact,
/// so this checks `has_synced()` up front instead (see that method's own
/// doc comment for why it's a real flag, not inferred from the revision).
/// An unsynced cache falls through to nodestore exactly as `cache: None`
/// would. `None` behaves exactly as before this parameter existed; callers
/// outside the listener's cache path still pass `None` (same scope as
/// `get`'s own cache parameter).
pub async fn list(
    storage: &mut StorageClient,
    cache: Option<&crate::cacher::store::SharedCache>,
    group: &str,
    version: &str,
    resource: &str,
    namespace: Option<&str>,
    label_selector: &str,
    field_selector: &str,
    limit: i64,
    continue_token: &str,
) -> Result<ListOutcome, Error> {
    list_at_revision(storage, cache, group, version, resource, namespace, label_selector, field_selector, limit, continue_token, 0).await
}

/// [`list`] with an optional etcd MVCC snapshot revision. A positive
/// revision bypasses the live watch cache and returns a consistent snapshot
/// from nodestore.
pub async fn list_at_revision(
    storage: &mut StorageClient,
    cache: Option<&crate::cacher::store::SharedCache>,
    group: &str,
    version: &str,
    resource: &str,
    namespace: Option<&str>,
    label_selector: &str,
    field_selector: &str,
    limit: i64,
    continue_token: &str,
    resource_version: i64,
) -> Result<ListOutcome, Error> {
    let Some(resolved) = resolve_resource(storage, group, version, resource).await? else {
        return Ok(ListOutcome::UnknownResource);
    };
    let kind = resolved.kind.as_str();
    let label_reqs = if label_selector.is_empty() { Vec::new() } else { selector::parse_label_selector(label_selector)? };
    let field_reqs = if field_selector.is_empty() { Vec::new() } else { selector::parse_field_selector(field_selector)? };
    selector::validate_field_selector(group, resource, &field_reqs)?;

    let group_version = if group.is_empty() { version.to_string() } else { format!("{group}/{version}") };
    // Shared by both the cache path and the direct-nodestore path below —
    // the cache registers one entry per whole `(group, version, resource)`
    // (`cacher::registry`'s own doc comment: "every namespace at once, not
    // one cache per namespace"), so a namespaced request still needs this
    // same prefix to scope the cache's own entries down to one namespace,
    // exactly as it already scopes the `Range` request on the fallback path.
    let prefix = keys::list_prefix(group, resource, namespace).into_bytes();

    // Real upstream itself doesn't serve a paginated request from its own
    // watch cache either — a consistent ordered range-scan-with-resume-point
    // is what the underlying store gives for free and an in-memory
    // unordered cache doesn't. A paginated request (real `limit`/`continue`,
    // not the default "everything") always goes straight to nodestore
    // below, same as an unsynced cache would.
    let paginated = limit > 0 || !continue_token.is_empty() || resource_version > 0;
    if let Some(cache) = cache {
        if cache.has_synced() && !paginated {
            let (entries, revision) = cache.list();
            let mut items = Vec::new();
            for (key, entry) in entries.iter().filter(|(key, _)| key.starts_with(&prefix)) {
                // Same real fix `get`'s own doc comment covers: a stored
                // object never carries `resourceVersion` as persisted
                // content, so every item in a `LIST` response needs it
                // stamped from its own live revision, the same way real
                // upstream does.
                let mut object = decrypt_and_decode_with_rotation(storage, group, resource, key, &entry.value, entry.mod_revision).await?;
                set_metadata_field(&mut object, "resourceVersion", Value::String(entry.mod_revision.to_string()));
                object = convert_to_requested_version(storage, group, version, kind, resolved.conversion_webhook.as_ref(), object).await?;
                if selector::object_matches(&object, &label_reqs, &field_reqs) {
                    items.push(object);
                }
            }
            return Ok(ListOutcome::Found(json!({
                "kind": list_kind(kind),
                "apiVersion": group_version,
                "metadata": {"resourceVersion": revision.to_string()},
                "items": items,
            })));
        }
    }

    let range_end = prefix_range_end(&prefix);
    // A `continue` token resumes from the exact key its own page left off
    // at (`encode_continue_token`'s own doc comment covers the "append a
    // single 0x00 byte" idiom that makes this the correct etcd range
    // start), at the same revision the listing began at — every page of
    // one listing sees a consistent snapshot, matching real upstream's
    // own pagination contract.
    let (start_key, at_revision) = if continue_token.is_empty() {
        (prefix, resource_version.max(0))
    } else {
        match decode_continue_token(continue_token) {
            Some((key, revision)) => (key, revision),
            None => return Ok(ListOutcome::InvalidContinueToken),
        }
    };
    let resp = storage.range(RangeRequest { key: start_key, range_end, limit: limit.max(0), revision: at_revision, ..Default::default() }).await?;
    let revision = resp.header.map(|h| h.revision).unwrap_or(at_revision);
    // Real upstream's own documented caveat applies here too: filtering by
    // label/field selector happens *after* the limited range fetch, so a
    // page can legitimately come back with fewer than `limit` items (or
    // even zero) despite there being more matching items on later pages —
    // this isn't a bug, it's the same trade-off a selector combined with
    // `limit` has against a real etcd-backed apiserver.
    let more = resp.more;
    // The successor marker `encode_continue_token`'s own doc comment
    // expects — appended *here*, not inside that function, so its own
    // internal `0x00` push stays purely about the encoding's key/revision
    // separator (see that function's doc comment for why the two
    // 0x00 bytes this produces when they land back to back is
    // deliberate, not a bug).
    let resume_key = resp.kvs.last().map(|kv| {
        let mut k = kv.key.clone();
        k.push(0);
        k
    });
    let mut items = Vec::with_capacity(resp.kvs.len());
    for kv in &resp.kvs {
        let mut object = decrypt_and_decode_with_rotation(storage, group, resource, &kv.key, &kv.value, kv.mod_revision).await?;
        set_metadata_field(&mut object, "resourceVersion", Value::String(kv.mod_revision.to_string()));
        object = convert_to_requested_version(storage, group, version, kind, resolved.conversion_webhook.as_ref(), object).await?;
        if selector::object_matches(&object, &label_reqs, &field_reqs) {
            items.push(object);
        }
    }

    let mut metadata = json!({"resourceVersion": revision.to_string()});
    if more {
        if let Some(resume_key) = resume_key {
            metadata["continue"] = json!(encode_continue_token(&resume_key, revision));
        }
    }

    Ok(ListOutcome::Found(json!({
        "kind": list_kind(kind),
        "apiVersion": group_version,
        "metadata": metadata,
        "items": items,
    })))
}
