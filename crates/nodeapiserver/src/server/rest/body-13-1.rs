    if group.is_empty() || group == "apiextensions.k8s.io" {
        return Ok(None);
    }
    let crds = list_stored_crds(storage).await?;
