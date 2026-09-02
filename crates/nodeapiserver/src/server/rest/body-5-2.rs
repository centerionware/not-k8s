    Ok(apiextensions::registry::discoverable_resources(crds.iter())
        .into_iter()
        .find(|candidate| candidate.group == group && candidate.version == version && candidate.resource == resource)
        .map(|candidate| candidate.namespaced))
