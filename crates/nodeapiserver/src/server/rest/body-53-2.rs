    Ok(match (resolved.schema, resolved.open_api_schema.as_ref()) {
        (Some(schema), _) => crate::patch::strategic_merge::apply(schema, existing, configuration),
        (None, Some(schema)) => apiextensions::schema_strategic_merge::apply(schema, existing, configuration),
        (None, None) => {
            let mut object = existing.clone();
            crate::patch::merge_patch::apply(&mut object, configuration);
            object
        }
    })
