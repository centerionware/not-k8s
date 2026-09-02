    Ok(PatchPrepareOutcome::Ready(
        patched,
        PatchContext { schema: resolved.schema, open_api_schema: resolved.open_api_schema, storage_open_api_schema: resolved.storage_open_api_schema, kind: resolved.kind, conversion_webhook: resolved.conversion_webhook, key, existing_kv, existing_object },
    ))
