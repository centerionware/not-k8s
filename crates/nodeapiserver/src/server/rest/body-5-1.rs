    if let Some(found) = codegen::api_resources_by_group_version()
        .get(&(group, version))
        .and_then(|resources| resources.iter().find(|candidate| candidate.resource == resource))
    {
        return Ok(Some(found.namespaced));
    }
    let crds = list_stored_crds(storage).await?;
