    let Some(resolved) = resolve_resource(storage, "", "v1", "pods").await? else {
        return Ok(UpdateOutcome::UnknownResource);
    };
    let key = keys::object_key("", "pods", Some(namespace), name);
    let existing_resp = storage.range(RangeRequest { key: key.clone().into_bytes(), ..Default::default() }).await?;
    let Some(existing_kv) = existing_resp.kvs.into_iter().next() else {
        return Ok(UpdateOutcome::ObjectNotFound);
    };
    let existing_object = decrypt_and_decode_with_rotation(storage, "", "pods", &existing_kv.key, &existing_kv.value, existing_kv.mod_revision).await?;
    let patched = match apply_patch(kind_of_patch, resolved.schema, None, &existing_object, patch_doc) {
        Ok(object) => object,
        Err(message) => return Ok(UpdateOutcome::Invalid(vec![message])),
    };
    let object = match restrict_ephemeral_container_update(&existing_object, &patched) {
        Ok(object) => object,
        Err(violations) => return Ok(UpdateOutcome::Invalid(violations)),
    };
