#[derive(Debug, PartialEq)]
pub enum BindOutcome {
    Bound,
    UnknownResource,
    ObjectNotFound,
    Conflict,
    Invalid(Vec<String>),
}

/// Implements the core Pod `binding` subresource used by the scheduler.
/// Real upstream's `BindingREST` validates the binding preconditions, sets
/// `spec.nodeName`, merges binding metadata, and marks the Pod scheduled in
/// one optimistic-concurrency write. Keeping that operation separate from
/// generic `update` matters: a Binding is a small request containing only a
/// target, not a replacement Pod object.
pub async fn bind_pod(
    storage: &mut StorageClient,
    namespace: &str,
    name: &str,
    body: &Value,
) -> Result<BindOutcome, Error> {
    let Some(resolved) = resolve_resource(storage, "", "v1", "pods").await? else {
        return Ok(BindOutcome::UnknownResource);
    };
    let key = keys::object_key("", "pods", Some(namespace), name);
    let existing_resp = storage
        .range(RangeRequest {
            key: key.clone().into_bytes(),
            ..Default::default()
        })
        .await?;
    let Some(existing_kv) = existing_resp.kvs.into_iter().next() else {
        return Ok(BindOutcome::ObjectNotFound);
    };
    let existing_object =
        decrypt_and_decode(storage, "", "pods", &existing_kv.key, &existing_kv.value)?;

    let mut violations = Vec::new();
    if let Some(binding_name) = body.pointer("/metadata/name").and_then(Value::as_str) {
        if binding_name != name {
            violations.push(format!("metadata.name: must be {name}"));
        }
    }
    if let Some(binding_namespace) = body.pointer("/metadata/namespace").and_then(Value::as_str) {
        if !binding_namespace.is_empty() && binding_namespace != namespace {
            violations.push("metadata.namespace: does not match the request URL".to_string());
        }
    }
    let Some(target_name) = body
        .pointer("/target/name")
        .and_then(Value::as_str)
        .filter(|name| !name.is_empty())
    else {
        violations.push("target.name: Required value".to_string());
        return Ok(BindOutcome::Invalid(violations));
    };
    if let Some(uid) = body.pointer("/metadata/uid").and_then(Value::as_str) {
        if existing_object
            .pointer("/metadata/uid")
            .and_then(Value::as_str)
            != Some(uid)
        {
            return Ok(BindOutcome::Conflict);
        }
    }
    if let Some(resource_version) = body
        .pointer("/metadata/resourceVersion")
        .and_then(Value::as_str)
    {
        if resource_version.parse::<i64>().ok() != Some(existing_kv.mod_revision) {
            return Ok(BindOutcome::Conflict);
        }
    }
    if existing_object
        .pointer("/metadata/deletionTimestamp")
        .is_some_and(|timestamp| !timestamp.is_null())
    {
        return Ok(BindOutcome::Conflict);
    }
    if existing_object
        .pointer("/spec/nodeName")
        .and_then(Value::as_str)
        .is_some_and(|node| !node.is_empty())
    {
        return Ok(BindOutcome::Conflict);
    }
    if existing_object
        .pointer("/spec/schedulingGates")
        .and_then(Value::as_array)
        .is_some_and(|gates| !gates.is_empty())
    {
        violations.push("spec.schedulingGates: Pod has scheduling gates".to_string());
    }
    if !violations.is_empty() {
        return Ok(BindOutcome::Invalid(violations));
    }

    let mut object = existing_object.clone();
    let Some(object_map) = object.as_object_mut() else {
        return Ok(BindOutcome::Invalid(vec![
            "Pod must be an object".to_string(),
        ]));
    };
    {
        let metadata = object_map.entry("metadata").or_insert_with(|| json!({}));
        let Some(metadata) = metadata.as_object_mut() else {
            return Ok(BindOutcome::Invalid(vec![
                "metadata must be an object".to_string(),
            ]));
        };
        for field in ["annotations", "labels"] {
            if let Some(values) = body.pointer(&format!("/metadata/{field}")) {
                let Some(values) = values.as_object() else {
                    return Ok(BindOutcome::Invalid(vec![format!(
                        "metadata.{field} must be an object"
                    )]));
                };
                let destination = metadata.entry(field).or_insert_with(|| json!({}));
                let Some(destination) = destination.as_object_mut() else {
                    return Ok(BindOutcome::Invalid(vec![format!(
                        "metadata.{field} must be an object"
                    )]));
                };
                for (key, value) in values {
                    destination.insert(key.clone(), value.clone());
                }
            }
        }
    }
    let spec = object_map.entry("spec").or_insert_with(|| json!({}));
    let Some(spec) = spec.as_object_mut() else {
        return Ok(BindOutcome::Invalid(vec![
            "spec must be an object".to_string(),
        ]));
    };
    spec.insert(
        "nodeName".to_string(),
        Value::String(target_name.to_string()),
    );

    let status = object_map.entry("status").or_insert_with(|| json!({}));
    let Some(status) = status.as_object_mut() else {
        return Ok(BindOutcome::Invalid(vec![
            "status must be an object".to_string(),
        ]));
    };
    let conditions = status.entry("conditions").or_insert_with(|| json!([]));
    let Some(conditions) = conditions.as_array_mut() else {
        return Ok(BindOutcome::Invalid(vec![
            "status.conditions must be an array".to_string(),
        ]));
    };
    let message = format!("Successfully assigned {namespace}/{name} to {target_name}");
    if let Some(condition) = conditions
        .iter_mut()
        .find(|condition| condition.get("type").and_then(Value::as_str) == Some("PodScheduled"))
    {
        condition["status"] = Value::String("True".to_string());
        condition["reason"] = Value::String("Scheduled".to_string());
        condition["message"] = Value::String(message);
        condition["lastTransitionTime"] = Value::String(now_rfc3339());
    } else {
        conditions.push(json!({
            "type": "PodScheduled",
            "status": "True",
            "reason": "Scheduled",
            "message": message,
            "lastTransitionTime": now_rfc3339(),
        }));
    }

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
        false,
    )
    .await?
    {
        UpdateOutcome::Updated(_) => Ok(BindOutcome::Bound),
        UpdateOutcome::Conflict => Ok(BindOutcome::Conflict),
        UpdateOutcome::Invalid(violations) => Ok(BindOutcome::Invalid(violations)),
        UpdateOutcome::UnknownResource | UpdateOutcome::ObjectNotFound => {
            Ok(BindOutcome::ObjectNotFound)
        }
        UpdateOutcome::MissingResourceVersion
        | UpdateOutcome::NamespaceMismatch
        | UpdateOutcome::UnsupportedPatchType => Ok(BindOutcome::Invalid(vec![
            "binding could not be persisted".to_string(),
        ])),
    }
}

/// The core Pod `ephemeralcontainers` subresource only exposes the Pod
/// object; its strategy permits changing `spec.ephemeralContainers` and
/// resets every other attempted change back to the stored Pod. Existing
/// ephemeral containers are immutable, so a caller may only append valid
/// new entries. This is the same boundary enforced by upstream's
/// `EphemeralContainersStrategy` before its normal optimistic-concurrency
/// store update.
fn restrict_ephemeral_container_update(
    existing: &Value,
    candidate: &Value,
) -> Result<Value, Vec<String>> {
    let existing_list = ephemeral_container_list(existing)?;
    let candidate_list = ephemeral_container_list(candidate)?;
    let mut violations = Vec::new();

    if candidate_list.len() < existing_list.len() {
        violations.push(
            "spec.ephemeralContainers: existing ephemeral containers may not be removed"
                .to_string(),
        );
    }
    for (index, old) in existing_list.iter().enumerate() {
        let old_name = old.get("name").and_then(Value::as_str);
        let replacement = old_name.and_then(|name| {
            candidate_list
                .iter()
                .find(|container| container.get("name").and_then(Value::as_str) == Some(name))
        });
        match replacement {
            None => violations.push(format!("spec.ephemeralContainers[{index}]: existing ephemeral containers may not be removed")),
            Some(new) if new != old => {
                violations.push(format!("spec.ephemeralContainers[{index}]: existing ephemeral containers may not be modified"));
            }
            Some(_) => {}
        }
    }

    let ordinary_names: std::collections::BTreeSet<&str> = existing
        .pointer("/spec/containers")
        .into_iter()
        .chain(existing.pointer("/spec/initContainers"))
        .filter_map(Value::as_array)
        .flat_map(|containers| containers.iter())
        .filter_map(|container| container.get("name").and_then(Value::as_str))
        .collect();
    let mut names = std::collections::BTreeSet::new();
    for (index, container) in candidate_list.iter().enumerate() {
        let Some(container) = container.as_object() else {
            violations.push(format!(
                "spec.ephemeralContainers[{index}]: must be an object"
            ));
            continue;
        };
        let Some(name) = container
            .get("name")
            .and_then(Value::as_str)
            .filter(|name| !name.is_empty())
        else {
            violations.push(format!(
                "spec.ephemeralContainers[{index}].name: Required value"
            ));
            continue;
        };
        for detail in crate::scheme::name_format::is_dns1123_label(name) {
            violations.push(format!("spec.ephemeralContainers[{index}].name: {detail}"));
        }
        if !names.insert(name) {
            violations.push(format!(
                "spec.ephemeralContainers[{index}].name: must be unique"
            ));
        }
        if ordinary_names.contains(name) {
            violations.push(format!("spec.ephemeralContainers[{index}].name: must not duplicate a regular or init container"));
        }
        if let Some(target) = container
            .get("targetContainerName")
            .and_then(Value::as_str)
            .filter(|target| !target.is_empty())
        {
            if !ordinary_names.contains(target) {
                violations.push(format!("spec.ephemeralContainers[{index}].targetContainerName: must name an existing regular or init container"));
            }
        }
        for field in [
            "ports",
            "resources",
            "lifecycle",
            "livenessProbe",
            "readinessProbe",
            "startupProbe",
        ] {
            if container.get(field).is_some_and(|value| !value.is_null()) {
                violations.push(format!("spec.ephemeralContainers[{index}].{field}: field is not allowed for an ephemeral container"));
            }
        }
    }
    if !violations.is_empty() {
        return Err(violations);
    }

    let mut object = existing.clone();
    let Some(spec) = object.get_mut("spec").and_then(Value::as_object_mut) else {
        return Err(vec!["spec: must be an object".to_string()]);
    };
    if candidate_list.is_empty() {
        spec.remove("ephemeralContainers");
    } else {
        spec.insert(
            "ephemeralContainers".to_string(),
            Value::Array(candidate_list.to_vec()),
        );
    }
    if candidate_list != existing_list {
        let generation = object
            .pointer("/metadata/generation")
            .and_then(Value::as_i64)
            .unwrap_or(0);
        set_metadata_field(
            &mut object,
            "generation",
            Value::Number((generation + 1).into()),
        );
    }
    Ok(object)
}

fn ephemeral_container_list(object: &Value) -> Result<&[Value], Vec<String>> {
    match object.pointer("/spec/ephemeralContainers") {
        None => Ok(&[]),
        Some(Value::Array(containers)) => Ok(containers),
        Some(_) => Err(vec![
            "spec.ephemeralContainers: must be an array".to_string(),
        ]),
    }
}

/// Reads a Pod through the `ephemeralcontainers` subresource. Upstream
/// returns the complete Pod because the subresource strategy only narrows
/// writes; the caller still needs the ordinary metadata and status fields
/// to observe the result.
pub async fn get_ephemeral_containers(
    storage: &mut StorageClient,
    namespace: &str,
    name: &str,
) -> Result<GetOutcome, Error> {
    get(storage, None, "", "v1", "pods", Some(namespace), name).await
}

/// Replaces a Pod through the `ephemeralcontainers` subresource. Only the
/// ephemeral-container list from `body` is retained; spec, status, and
/// ordinary metadata changes are discarded by the subresource strategy.
pub async fn update_ephemeral_containers(
    storage: &mut StorageClient,
    namespace: &str,
    name: &str,
    body: &Value,
    dry_run: bool,
    field_manager: Option<&str>,
    validate: impl FnOnce(&Value) -> Result<(), Vec<String>>,
) -> Result<UpdateOutcome, Error> {
    let Some(resolved) = resolve_resource(storage, "", "v1", "pods").await? else {
        return Ok(UpdateOutcome::UnknownResource);
    };
    let key = keys::object_key("", "pods", Some(namespace), name);
    let existing_resp = storage
        .range(RangeRequest {
            key: key.clone().into_bytes(),
            ..Default::default()
        })
        .await?;
    let Some(existing_kv) = existing_resp.kvs.into_iter().next() else {
        return Ok(UpdateOutcome::ObjectNotFound);
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
    let body =
        convert_to_requested_version(storage, "", "v1", &resolved.kind, None, body.clone()).await?;
    let object = match restrict_ephemeral_container_update(&existing_object, &body) {
        Ok(object) => object,
        Err(violations) => return Ok(UpdateOutcome::Invalid(violations)),
    };
    if let Err(violations) = validate(&object) {
        return Ok(UpdateOutcome::Invalid(violations));
    }
    persist_update(
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
        dry_run,
        None,
        field_manager,
        "ephemeralcontainers",
        false,
        false,
    )
    .await
}

/// Applies a JSON/merge/strategic patch through the Pod
/// `ephemeralcontainers` subresource. The patch is evaluated against the
/// complete current Pod, then only its resulting ephemeral-container list
/// is retained, matching upstream's reset-fields strategy.
pub async fn patch_ephemeral_containers(
    storage: &mut StorageClient,
    namespace: &str,
    name: &str,
    kind_of_patch: PatchKind,
    patch_doc: &Value,
    dry_run: bool,
    field_manager: Option<&str>,
    validate: impl FnOnce(&Value) -> Result<(), Vec<String>>,
) -> Result<UpdateOutcome, Error> {
    let Some(resolved) = resolve_resource(storage, "", "v1", "pods").await? else {
        return Ok(UpdateOutcome::UnknownResource);
    };
    let key = keys::object_key("", "pods", Some(namespace), name);
    let existing_resp = storage
        .range(RangeRequest {
            key: key.clone().into_bytes(),
            ..Default::default()
        })
        .await?;
    let Some(existing_kv) = existing_resp.kvs.into_iter().next() else {
        return Ok(UpdateOutcome::ObjectNotFound);
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
    let patched = match apply_patch(
        kind_of_patch,
        resolved.schema,
        None,
        &existing_object,
        patch_doc,
    ) {
        Ok(object) => object,
        Err(message) => return Ok(UpdateOutcome::Invalid(vec![message])),
    };
    let object = match restrict_ephemeral_container_update(&existing_object, &patched) {
        Ok(object) => object,
        Err(violations) => return Ok(UpdateOutcome::Invalid(violations)),
    };
    if let Err(violations) = validate(&object) {
        return Ok(UpdateOutcome::Invalid(violations));
    }
    persist_update(
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
        dry_run,
        None,
        field_manager,
        "ephemeralcontainers",
        false,
        false,
    )
    .await
}
