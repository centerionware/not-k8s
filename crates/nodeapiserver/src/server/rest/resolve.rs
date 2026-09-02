struct ResolvedResource {
    kind: String,
    /// `Some(proto message name)` for a built-in; `None` for a CRD.
    schema: Option<&'static str>,
    open_api_schema: Option<Value>,
    /// Additional field-selector paths declared by an established CRD. An
    /// empty list is the built-in/default CRD behavior: only metadata fields
    /// are selectable.
    selectable_fields: Vec<String>,
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
                selectable_fields: Vec::new(),
                storage_open_api_schema: None,
                has_status_subresource: true,
                conversion_webhook: None,
            }),
        );
    }
    Ok(resolve_crd(storage, group, version, resource)
        .await?
        .map(|r| ResolvedResource {
            kind: r.kind,
            schema: None,
            open_api_schema: r.open_api_schema,
            selectable_fields: r.selectable_fields,
            storage_open_api_schema: r.storage_open_api_schema,
            has_status_subresource: r.has_status_subresource,
            conversion_webhook: r.conversion_webhook,
        }))
}

/// Resolve the OpenAPI schema used to declare CEL mutation object aliases.
/// Built-in schemas come from the same vendored document advertised by
/// `/openapi/v3`; CRD schemas come from their established version directly.
/// Built-in references are expanded here so the CEL environment can register
/// names such as `Object.spec.containers` without duplicating schema lookup
/// rules in the admission layer.
pub async fn mutation_openapi_schema(
    storage: &mut StorageClient,
    group: &str,
    version: &str,
    resource: &str,
) -> Result<Option<Value>, Error> {
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
    let Some(schemas) = document
        .pointer("/components/schemas")
        .and_then(Value::as_object)
    else {
        return Ok(None);
    };
    let Some(root) = schemas.get(schema_name) else {
        return Ok(None);
    };
    Ok(Some(expand_openapi_refs(
        root,
        schemas,
        &mut BTreeSet::new(),
    )))
}

fn expand_openapi_refs(
    value: &Value,
    schemas: &Map<String, Value>,
    active: &mut BTreeSet<String>,
) -> Value {
    match value {
        Value::Array(values) => Value::Array(
            values
                .iter()
                .map(|value| expand_openapi_refs(value, schemas, active))
                .collect(),
        ),
        Value::Object(object) => {
            if let Some(reference) = object
                .get("$ref")
                .and_then(Value::as_str)
                .and_then(|reference| reference.strip_prefix("#/components/schemas/"))
            {
                if let Some(target) = schemas.get(reference) {
                    if active.insert(reference.to_string()) {
                        let mut expanded = expand_openapi_refs(target, schemas, active);
                        active.remove(reference);
                        if let Value::Object(expanded_object) = &mut expanded {
                            for (key, value) in object {
                                if key != "$ref" {
                                    expanded_object.insert(
                                        key.clone(),
                                        expand_openapi_refs(value, schemas, active),
                                    );
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
            Value::Object(
                object
                    .iter()
                    .map(|(key, value)| (key.clone(), expand_openapi_refs(value, schemas, active)))
                    .collect(),
            )
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
    let mut objects = apiextensions::conversion::convert(
        storage,
        group,
        conversion_webhook,
        &conversion_webhook.storage_version,
        vec![object],
    )
    .await
    .map_err(|error| Error::InvalidProtobufRequest(error.to_string()))?;
    objects.pop().ok_or_else(|| {
        Error::InvalidProtobufRequest("conversion webhook returned no object".to_string())
    })
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
            .map(|api_version| {
                api_version
                    .rsplit_once('/')
                    .map_or(api_version, |(_, version)| version)
            });
        if source_version != Some(version) {
            let mut objects = apiextensions::conversion::convert(
                storage,
                group,
                conversion_webhook,
                version,
                vec![object],
            )
            .await
            .map_err(|error| Error::InvalidProtobufRequest(error.to_string()))?;
            objects.pop().ok_or_else(|| {
                Error::InvalidProtobufRequest("conversion webhook returned no object".to_string())
            })?
        } else {
            object
        }
    } else {
        object
    };
    Ok(crate::scheme::conversion::to_version(
        group, version, kind, object,
    ))
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
            .map(|violation| {
                format!(
                    "{}: expected type {}, got {}",
                    violation.path, violation.expected, violation.actual_kind
                )
            }),
    );
    violations.extend(apiextensions::schema_validation::validate_constraints(
        schema, &object,
    ));
    violations.extend(metadata_format_violations(&object));
    if violations.is_empty() {
        Ok(object)
    } else {
        Err(violations)
    }
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
    Ok(resolve_dynamic_resource(storage, group, version, resource)
        .await?
        .map(|r| r.kind))
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
    let mut objects = Vec::with_capacity(resp.kvs.len());
    for kv in resp.kvs {
        objects.push(
            decrypt_and_decode_with_rotation(
                storage,
                "apiextensions.k8s.io",
                "customresourcedefinitions",
                &kv.key,
                &kv.value,
                kv.mod_revision,
            )
            .await?,
        );
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
/// The plain helper is retained for synchronous callers such as watch-event
/// formatting. Async REST reads use [`decrypt_and_decode_with_rotation`],
/// which honors the transformer's stale-key signal after decoding.
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
                tracing::debug!(
                    group,
                    resource,
                    revision,
                    "storage: re-encrypted a value with the primary key"
                );
            }
            Ok(_) => {
                tracing::debug!(
                    group,
                    resource,
                    revision,
                    "storage: skipped stale-key rewrite after a concurrent update"
                );
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
