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
