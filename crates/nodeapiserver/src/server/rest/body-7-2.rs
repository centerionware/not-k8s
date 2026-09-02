    Ok(resolve_crd(storage, group, version, resource)
        .await?
        .map(|r| ResolvedResource { kind: r.kind, schema: None, open_api_schema: r.open_api_schema, storage_open_api_schema: r.storage_open_api_schema, has_status_subresource: r.has_status_subresource, conversion_webhook: r.conversion_webhook }))
