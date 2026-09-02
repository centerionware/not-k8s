    let Some(resolved) = resolve_resource(storage, group, version, resource).await? else {
        return Ok(None);
    };
