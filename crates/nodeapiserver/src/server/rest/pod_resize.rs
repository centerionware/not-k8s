// The core Pod `resize` subresource.
//
// Kubernetes treats Pod resource changes as a narrow update: only each
// container's `resources` and `resizePolicy` fields may change. The request
// still returns the complete Pod, and the ordinary MVCC/validation/storage
// path remains responsible for the final write.

/// Reads the complete Pod through the `resize` subresource. The subresource
/// has a distinct route, but its response is the same Pod object returned by
/// the normal Pod GET endpoint.
pub async fn get_pod_resize(
    storage: &mut StorageClient,
    namespace: &str,
    name: &str,
) -> Result<GetOutcome, Error> {
    get(storage, None, "", "v1", "pods", Some(namespace), name).await
}

/// Replaces a Pod through the `resize` subresource. The submitted object
/// must identify the current resource version, while only resize-owned
/// fields are copied onto the stored Pod.
pub async fn update_pod_resize(
    storage: &mut StorageClient,
    namespace: &str,
    name: &str,
    body: &Value,
    dry_run: bool,
    field_manager: Option<&str>,
) -> Result<UpdateOutcome, Error> {
    let Some(state) = load_pod_resize_state(storage, namespace, name).await? else {
        return Ok(UpdateOutcome::ObjectNotFound);
    };
    let Some(submitted_rv) = body
        .pointer("/metadata/resourceVersion")
        .and_then(Value::as_str)
        .and_then(|value| value.parse::<i64>().ok())
    else {
        return Ok(UpdateOutcome::MissingResourceVersion);
    };
    if submitted_rv != state.existing_kv.mod_revision {
        return Ok(UpdateOutcome::Conflict);
    }
    if body
        .pointer("/metadata/namespace")
        .and_then(Value::as_str)
        .is_some_and(|body_namespace| !body_namespace.is_empty() && body_namespace != namespace)
    {
        return Ok(UpdateOutcome::NamespaceMismatch);
    }

    let violations = validate_pod_resize_body(&state.resolved, body);
    if !violations.is_empty() {
        return Ok(UpdateOutcome::Invalid(violations));
    }
    let object = match restrict_resize_update(&state.existing_object_for_request, body) {
        Ok(object) => object,
        Err(violations) => return Ok(UpdateOutcome::Invalid(violations)),
    };
    let object = default_pod_resize_object(&state.resolved, object)?;

    persist_update(
        storage,
        state.resolved.schema,
        state.resolved.open_api_schema.as_ref(),
        state.resolved.storage_open_api_schema.as_ref(),
        &state.resolved.kind,
        "",
        "v1",
        "pods",
        state.key,
        &state.existing_kv,
        &state.existing_object,
        Some(namespace),
        object,
        dry_run,
        state.resolved.conversion_webhook,
        field_manager,
        "resize",
        state.resolved.has_status_subresource,
        false,
    )
    .await
}

/// Applies a normal JSON/merge/strategic patch, then narrows the resulting
/// object to the fields owned by the resize subresource before persisting it.
pub async fn patch_pod_resize(
    storage: &mut StorageClient,
    namespace: &str,
    name: &str,
    kind_of_patch: PatchKind,
    patch_doc: &Value,
    dry_run: bool,
    field_manager: Option<&str>,
) -> Result<UpdateOutcome, Error> {
    match patch_prepare(
        storage,
        "",
        "v1",
        "pods",
        Some(namespace),
        name,
        kind_of_patch,
        patch_doc,
    )
    .await?
    {
        PatchPrepareOutcome::Ready(candidate, context) => {
            let object = match restrict_resize_update(&context.existing_object, &candidate) {
                Ok(object) => object,
                Err(violations) => return Ok(UpdateOutcome::Invalid(violations)),
            };
            patch_persist_with_manager(
                storage,
                "",
                "v1",
                "pods",
                Some(namespace),
                name,
                context,
                object,
                dry_run,
                field_manager,
            )
            .await
        }
        PatchPrepareOutcome::UnknownResource => Ok(UpdateOutcome::UnknownResource),
        PatchPrepareOutcome::ObjectNotFound => Ok(UpdateOutcome::ObjectNotFound),
        PatchPrepareOutcome::Invalid(violations) => Ok(UpdateOutcome::Invalid(violations)),
    }
}

struct PodResizeState {
    resolved: ResolvedResource,
    key: String,
    existing_kv: mvccpb::KeyValue,
    existing_object: Value,
    existing_object_for_request: Value,
}

async fn load_pod_resize_state(
    storage: &mut StorageClient,
    namespace: &str,
    name: &str,
) -> Result<Option<PodResizeState>, Error> {
    let Some(resolved) = resolve_resource(storage, "", "v1", "pods").await? else {
        return Ok(None);
    };
    let key = keys::object_key("", "pods", Some(namespace), name);
    let response = storage
        .range(RangeRequest {
            key: key.clone().into_bytes(),
            ..Default::default()
        })
        .await?;
    let Some(existing_kv) = response.kvs.into_iter().next() else {
        return Ok(None);
    };
    let existing_object = decrypt_and_decode_with_rotation(
        storage,
        "",
        "pods",
        &existing_kv.key,
        &existing_kv.value,
        existing_kv.mod_revision,
    )
    .await?;
    let existing_object_for_request = convert_to_requested_version(
        storage,
        "",
        "v1",
        &resolved.kind,
        resolved.conversion_webhook.as_ref(),
        existing_object.clone(),
    )
    .await?;
    Ok(Some(PodResizeState {
        resolved,
        key,
        existing_kv,
        existing_object,
        existing_object_for_request,
    }))
}

fn validate_pod_resize_body(resolved: &ResolvedResource, body: &Value) -> Vec<String> {
    let Some(schema) = resolved.schema else {
        return vec!["Pod resize requires the built-in Pod schema".to_string()];
    };
    let mut violations = crate::scheme::validation::validate_required(schema, body)
        .into_iter()
        .map(|violation| format!("{}: Required value", violation.path))
        .collect::<Vec<_>>();
    violations.extend(
        crate::scheme::validation::validate_types(schema, body)
            .into_iter()
            .map(|violation| {
                format!(
                    "{}: expected type {}, got {}",
                    violation.path, violation.expected, violation.actual_kind
                )
            }),
    );
    violations.extend(crate::scheme::validation::validate_openapi_constraints(
        "",
        "v1",
        &resolved.kind,
        body,
    ));
    violations
}

fn default_pod_resize_object(resolved: &ResolvedResource, object: Value) -> Result<Value, Error> {
    let Some(schema) = resolved.schema else {
        return Err(Error::UnknownResource);
    };
    Ok(defaulting::apply_builtin_defaults(
        "",
        "v1",
        &resolved.kind,
        defaulting::apply_defaults(schema, &object),
    ))
}

/// Keeps the stored Pod intact except for the resize strategy's four allowed
/// field families: regular/init container resources and resize policies.
fn restrict_resize_update(existing: &Value, candidate: &Value) -> Result<Value, Vec<String>> {
    let mut object = existing.clone();
    for containers in ["containers", "initContainers"] {
        let Some(candidate_list) = candidate.pointer(&format!("/spec/{containers}")) else {
            continue;
        };
        let existing_list = existing
            .pointer(&format!("/spec/{containers}"))
            .and_then(Value::as_array)
            .ok_or_else(|| vec![format!("spec.{containers}: must be an array")])?;
        let candidate_list = candidate_list
            .as_array()
            .ok_or_else(|| vec![format!("spec.{containers}: must be an array")])?;
        let mut violations = Vec::new();
        if candidate_list.len() != existing_list.len() {
            violations.push(format!(
                "spec.{containers}: container count may not change through the resize subresource"
            ));
        }
        for (index, existing_container) in existing_list.iter().enumerate() {
            let Some(candidate_container) = candidate_list.get(index) else {
                continue;
            };
            let Some(existing_name) = existing_container.get("name").and_then(Value::as_str) else {
                violations.push(format!(
                    "spec.{containers}[{index}].name: missing from stored Pod"
                ));
                continue;
            };
            let Some(candidate_container) = candidate_container.as_object() else {
                violations.push(format!("spec.{containers}[{index}]: must be an object"));
                continue;
            };
            if candidate_container.get("name").and_then(Value::as_str) != Some(existing_name) {
                violations.push(format!(
                    "spec.{containers}[{index}].name: container order and names may not change through the resize subresource"
                ));
            }
            let Some(destination) = object
                .pointer_mut(&format!("/spec/{containers}/{index}"))
                .and_then(Value::as_object_mut)
            else {
                violations.push(format!(
                    "spec.{containers}[{index}]: missing from stored Pod"
                ));
                continue;
            };
            for field in ["resources", "resizePolicy"] {
                if let Some(value) = candidate_container.get(field) {
                    destination.insert(field.to_string(), value.clone());
                }
            }
        }
        if !violations.is_empty() {
            return Err(violations);
        }
    }
    Ok(object)
}

#[cfg(test)]
mod pod_resize_tests {
    use super::restrict_resize_update;
    use serde_json::json;

    #[test]
    fn resize_only_changes_resources_and_policy() {
        let existing = json!({
            "metadata": {"name": "demo", "labels": {"keep": "yes"}},
            "spec": {"nodeName": "node-a", "containers": [{
                "name": "app", "image": "old", "resources": {"limits": {"memory": "128Mi"}}
            }]},
            "status": {"phase": "Running"}
        });
        let candidate = json!({
            "metadata": {"name": "demo", "labels": {"keep": "no"}},
            "spec": {"nodeName": "node-b", "containers": [{
                "name": "app", "image": "new", "resources": {"limits": {"memory": "256Mi"}},
                "resizePolicy": [{"resourceName": "memory", "restartPolicy": "NotRequired"}]
            }]},
            "status": {"phase": "Failed"}
        });
        let resized = restrict_resize_update(&existing, &candidate).expect("valid resize");
        assert_eq!(resized["metadata"]["labels"]["keep"], "yes");
        assert_eq!(resized["spec"]["nodeName"], "node-a");
        assert_eq!(resized["spec"]["containers"][0]["image"], "old");
        assert_eq!(
            resized["spec"]["containers"][0]["resources"]["limits"]["memory"],
            "256Mi"
        );
        assert_eq!(resized["status"]["phase"], "Running");
    }

    #[test]
    fn resize_rejects_container_replacement() {
        let existing = json!({"spec": {"containers": [{"name": "app", "image": "old"}]}});
        let candidate = json!({"spec": {"containers": [{"name": "other", "resources": {}}]}});
        let errors = restrict_resize_update(&existing, &candidate).expect_err("name change");
        assert!(errors.iter().any(|error| error.contains("order and names")));
    }
}
