    let listed = list(
        storage,
        None,
        group,
        version,
        resource,
        namespace,
        label_selector,
        field_selector,
        0,
        "",
    )
    .await?;
    let ListOutcome::Found(list_value) = listed else {
        return Ok(DeleteCollectionOutcome::UnknownResource);
    };
