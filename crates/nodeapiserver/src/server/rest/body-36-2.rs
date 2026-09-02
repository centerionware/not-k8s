    match persist_update(
        storage,
        resolved.schema,
        None,
        None,
        &resolved.kind,
        "",
        "v1",
        "pods",
        key,
        &existing_kv,
        &existing_object,
        Some(namespace),
        object,
        false,
        None,
        None,
        "",
        false,
    )
    .await?
    {
        UpdateOutcome::Updated(_) => Ok(BindOutcome::Bound),
        UpdateOutcome::Conflict => Ok(BindOutcome::Conflict),
        UpdateOutcome::Invalid(violations) => Ok(BindOutcome::Invalid(violations)),
        UpdateOutcome::UnknownResource | UpdateOutcome::ObjectNotFound => Ok(BindOutcome::ObjectNotFound),
        UpdateOutcome::MissingResourceVersion | UpdateOutcome::NamespaceMismatch | UpdateOutcome::UnsupportedPatchType => Ok(BindOutcome::Invalid(vec!["binding could not be persisted".to_string()])),
    }
