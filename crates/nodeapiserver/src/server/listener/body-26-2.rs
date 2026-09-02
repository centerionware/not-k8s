    Ok(Some(rest::DeletePreconditions {
        resource_version: string_field("resourceVersion")?,
        uid: string_field("uid")?,
    }))
