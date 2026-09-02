    let Some(metadata) = metadata else {
        return;
    };
    let Ok(mut metadata) = metadata.lock() else {
        return;
    };
    metadata.warnings.extend(outcome.warnings.iter().cloned());
