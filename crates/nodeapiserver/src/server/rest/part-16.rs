
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
