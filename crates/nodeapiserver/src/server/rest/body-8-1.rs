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
