    if let Some(kind) = resolve_kind(group, version, resource) {
        return Ok(protobuf::schema_for_gvk(group, version, kind).map(|schema| ResolvedResource { kind: kind.to_string(), schema: Some(schema), open_api_schema: None, storage_open_api_schema: None, has_status_subresource: true, conversion_webhook: None }));
    }
