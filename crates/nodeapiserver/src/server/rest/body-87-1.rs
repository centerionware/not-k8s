    let listed = list_delete_collection(storage, group, version, resource, namespace, label_selector, field_selector).await?;
    let DeleteCollectionOutcome::Deleted(list_value) = listed else {
        return Ok(DeleteCollectionOutcome::UnknownResource);
    };
    let items = list_value["items"].as_array().cloned().unwrap_or_default();
    for item in &items {
        let Some(name) = item.pointer("/metadata/name").and_then(Value::as_str) else { continue };
        match delete(storage, group, version, resource, namespace, name).await? {
            DeleteOutcome::ObjectNotFound => {}
            DeleteOutcome::Deleted(_) | DeleteOutcome::UnknownResource | DeleteOutcome::PreconditionFailed => {}
        }
    }
