    let mut static_matches = codegen::api_resources::API_RESOURCES
        .iter()
        .filter(|resource| resource.group == group && resource.kind == kind)
        .collect::<Vec<_>>();
    static_matches.sort_by(|left, right| super::version_compare::compare_kube_aware_versions(&right.version, &left.version));
    if let Some(resource) = static_matches.into_iter().next() {
        return Ok(Some((resource.group.to_string(), resource.version.to_string(), resource.resource.to_string(), resource.namespaced)));
    }

    let mut dynamic_matches = apiextensions::registry::discoverable_resources(list_stored_crds(storage).await?.iter())
        .into_iter()
        .filter(|resource| resource.group == group && resource.kind == kind)
        .collect::<Vec<_>>();
    dynamic_matches.sort_by(|left, right| super::version_compare::compare_kube_aware_versions(&right.version, &left.version));
