    Ok(dynamic_matches.into_iter().next().map(|resource| (resource.group, resource.version, resource.resource, resource.namespaced)))
