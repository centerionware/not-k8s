/// The virtual `autoscaling/v1 Scale` resource exposed by the built-in
/// workload scale subresources. Scale reads and writes are translated to
/// the parent object's `spec.replicas`; the Scale object itself is never
/// persisted in nodestore.
#[derive(Debug, PartialEq)]
pub enum ScaleOutcome {
    Found(Value),
    Updated(Value),
    UnknownResource,
    ObjectNotFound,
    MissingResourceVersion,
    Conflict,
    Invalid(Vec<String>),
}

/// Built-in resources whose upstream API exposes the `autoscaling/v1`
/// Scale subresource. These are the resources represented by the vendored
/// OpenAPI paths; a CRD's independently configured scale mapping is a
/// separate runtime-schema feature and is not implied by this list.
pub fn supports_scale(group: &str, version: &str, resource: &str) -> bool {
    (group.is_empty() && version == "v1" && resource == "replicationcontrollers")
        || (group == "apps"
            && version == "v1"
            && matches!(resource, "deployments" | "replicasets" | "statefulsets"))
}

/// Build the `autoscaling/v1 Scale` representation for one stored parent
/// object. The parent is already read through the normal REST path, so its
/// metadata carries the current MVCC resourceVersion.
pub fn scale_from_parent(parent: &Value) -> Result<Value, String> {
    let name = parent
        .pointer("/metadata/name")
        .and_then(Value::as_str)
        .filter(|name| !name.is_empty())
        .ok_or_else(|| "the scaled object has no metadata.name".to_string())?;
    let mut metadata = Map::new();
    metadata.insert("name".to_string(), Value::String(name.to_string()));
    for field in ["namespace", "uid", "resourceVersion"] {
        if let Some(value) = parent
            .pointer(&format!("/metadata/{field}"))
            .filter(|value| !value.is_null())
        {
            metadata.insert(field.to_string(), value.clone());
        }
    }

    let replicas = parent
        .pointer("/spec/replicas")
        .and_then(Value::as_i64)
        .unwrap_or(1);
    let status_replicas = parent
        .pointer("/status/replicas")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let mut status = Map::new();
    status.insert(
        "replicas".to_string(),
        Value::Number(status_replicas.into()),
    );
    if let Some(selector) = scale_selector(parent) {
        status.insert("selector".to_string(), Value::String(selector));
    }

    Ok(json!({
        "apiVersion": "autoscaling/v1",
        "kind": "Scale",
        "metadata": metadata,
        "spec": {"replicas": replicas},
        "status": status,
    }))
}

/// Read a built-in workload's virtual Scale object.
pub async fn get_scale(
    storage: &mut StorageClient,
    group: &str,
    version: &str,
    resource: &str,
    namespace: Option<&str>,
    name: &str,
) -> Result<ScaleOutcome, Error> {
    if !supports_scale(group, version, resource) {
        return Ok(ScaleOutcome::UnknownResource);
    }
    match get(storage, None, group, version, resource, namespace, name).await? {
        GetOutcome::Found(parent) => Ok(ScaleOutcome::Found(
            scale_from_parent(&parent).map_err(|error| Error::InvalidProtobufRequest(error))?,
        )),
        GetOutcome::UnknownResource => Ok(ScaleOutcome::UnknownResource),
        GetOutcome::ObjectNotFound => Ok(ScaleOutcome::ObjectNotFound),
    }
}

/// Update the parent object's replica count from a virtual Scale object.
/// The caller may request `dry_run=All`; preparation and validation still
/// happen, but the parent write is skipped.
pub async fn update_scale(
    storage: &mut StorageClient,
    group: &str,
    version: &str,
    resource: &str,
    namespace: Option<&str>,
    name: &str,
    body: &Value,
    dry_run: bool,
) -> Result<ScaleOutcome, Error> {
    if !supports_scale(group, version, resource) {
        return Ok(ScaleOutcome::UnknownResource);
    }
    let current = match get(storage, None, group, version, resource, namespace, name).await? {
        GetOutcome::Found(parent) => parent,
        GetOutcome::UnknownResource => return Ok(ScaleOutcome::UnknownResource),
        GetOutcome::ObjectNotFound => return Ok(ScaleOutcome::ObjectNotFound),
    };
    let current_rv = current
        .pointer("/metadata/resourceVersion")
        .and_then(Value::as_str)
        .unwrap_or("");
    let violations = scale_metadata_violations(body, namespace, name);
    if !violations.is_empty() {
        return Ok(ScaleOutcome::Invalid(violations));
    }
    let replicas = match scale_replicas(body) {
        Ok(replicas) => replicas,
        Err(error) => return Ok(ScaleOutcome::Invalid(vec![error])),
    };
    let Some(submitted_rv) = scale_resource_version(body) else {
        return Ok(ScaleOutcome::MissingResourceVersion);
    };
    if submitted_rv != current_rv {
        return Ok(ScaleOutcome::Conflict);
    }

    let parent = parent_with_replicas(&current, replicas).map_err(Error::InvalidProtobufRequest)?;
    match update_with_options(
        storage, group, version, resource, namespace, name, &parent, dry_run,
    )
    .await?
    {
        UpdateOutcome::Updated(updated) => Ok(ScaleOutcome::Updated(
            scale_from_parent(&updated).map_err(Error::InvalidProtobufRequest)?,
        )),
        UpdateOutcome::UnknownResource => Ok(ScaleOutcome::UnknownResource),
        UpdateOutcome::ObjectNotFound => Ok(ScaleOutcome::ObjectNotFound),
        UpdateOutcome::MissingResourceVersion => Ok(ScaleOutcome::MissingResourceVersion),
        UpdateOutcome::Conflict => Ok(ScaleOutcome::Conflict),
        UpdateOutcome::NamespaceMismatch => Ok(ScaleOutcome::Invalid(vec![
            "metadata.namespace: does not match the request URL".to_string(),
        ])),
        UpdateOutcome::Invalid(violations) => Ok(ScaleOutcome::Invalid(violations)),
        UpdateOutcome::UnsupportedPatchType => Ok(ScaleOutcome::Invalid(vec![
            "the Scale update could not be persisted".to_string(),
        ])),
    }
}

/// Apply an ordinary JSON/merge/strategic patch to the virtual Scale, then
/// persist only its replica count through the parent resource's update path.
pub async fn patch_scale(
    storage: &mut StorageClient,
    group: &str,
    version: &str,
    resource: &str,
    namespace: Option<&str>,
    name: &str,
    kind_of_patch: PatchKind,
    patch_doc: &Value,
    dry_run: bool,
) -> Result<ScaleOutcome, Error> {
    if !supports_scale(group, version, resource) {
        return Ok(ScaleOutcome::UnknownResource);
    }
    let current = match get(storage, None, group, version, resource, namespace, name).await? {
        GetOutcome::Found(parent) => parent,
        GetOutcome::UnknownResource => return Ok(ScaleOutcome::UnknownResource),
        GetOutcome::ObjectNotFound => return Ok(ScaleOutcome::ObjectNotFound),
    };
    let current_rv = current
        .pointer("/metadata/resourceVersion")
        .and_then(Value::as_str)
        .unwrap_or("");
    let existing_scale = match scale_from_parent(&current) {
        Ok(scale) => scale,
        Err(error) => return Ok(ScaleOutcome::Invalid(vec![error])),
    };
    let patched_scale = match apply_scale_patch(kind_of_patch, &existing_scale, patch_doc) {
        Ok(scale) => scale,
        Err(error) => return Ok(ScaleOutcome::Invalid(vec![error])),
    };
    let violations = scale_metadata_violations(&patched_scale, namespace, name);
    if !violations.is_empty() {
        return Ok(ScaleOutcome::Invalid(violations));
    }
    let replicas = match scale_replicas(&patched_scale) {
        Ok(replicas) => replicas,
        Err(error) => return Ok(ScaleOutcome::Invalid(vec![error])),
    };
    if let Some(submitted_rv) = scale_resource_version(&patched_scale) {
        if submitted_rv != current_rv {
            return Ok(ScaleOutcome::Conflict);
        }
    }

    let parent = parent_with_replicas(&current, replicas).map_err(Error::InvalidProtobufRequest)?;
    match update_with_options(
        storage, group, version, resource, namespace, name, &parent, dry_run,
    )
    .await?
    {
        UpdateOutcome::Updated(updated) => Ok(ScaleOutcome::Updated(
            scale_from_parent(&updated).map_err(Error::InvalidProtobufRequest)?,
        )),
        UpdateOutcome::UnknownResource => Ok(ScaleOutcome::UnknownResource),
        UpdateOutcome::ObjectNotFound => Ok(ScaleOutcome::ObjectNotFound),
        UpdateOutcome::MissingResourceVersion => Ok(ScaleOutcome::MissingResourceVersion),
        UpdateOutcome::Conflict => Ok(ScaleOutcome::Conflict),
        UpdateOutcome::NamespaceMismatch => Ok(ScaleOutcome::Invalid(vec![
            "metadata.namespace: does not match the request URL".to_string(),
        ])),
        UpdateOutcome::Invalid(violations) => Ok(ScaleOutcome::Invalid(violations)),
        UpdateOutcome::UnsupportedPatchType => Ok(ScaleOutcome::Invalid(vec![
            "the Scale patch could not be persisted".to_string(),
        ])),
    }
}

fn scale_resource_version(value: &Value) -> Option<&str> {
    value
        .pointer("/metadata/resourceVersion")
        .and_then(Value::as_str)
        .filter(|version| !version.is_empty())
}

fn scale_metadata_violations(value: &Value, namespace: Option<&str>, name: &str) -> Vec<String> {
    let Some(metadata) = value.get("metadata") else {
        return Vec::new();
    };
    let Some(metadata) = metadata.as_object() else {
        return vec!["metadata: must be an object".to_string()];
    };
    let mut violations = Vec::new();
    if let Some(body_name) = metadata.get("name") {
        if body_name.as_str() != Some(name) {
            violations.push(format!("metadata.name: must be {name}"));
        }
    }
    if let Some(body_namespace) = metadata.get("namespace") {
        if let Some(namespace) = namespace {
            if body_namespace.as_str().is_some_and(|body_namespace| {
                !body_namespace.is_empty() && body_namespace != namespace
            }) {
                violations.push("metadata.namespace: does not match the request URL".to_string());
            }
        }
    }
    violations
}

fn scale_replicas(value: &Value) -> Result<i64, String> {
    let Some(replicas) = value.pointer("/spec/replicas").and_then(Value::as_i64) else {
        return Err("spec.replicas: Required value must be an integer".to_string());
    };
    if replicas < 0 {
        return Err("spec.replicas: must be greater than or equal to 0".to_string());
    }
    Ok(replicas)
}

fn parent_with_replicas(current: &Value, replicas: i64) -> Result<Value, String> {
    let mut parent = current.clone();
    let previous = parent
        .pointer("/spec/replicas")
        .and_then(Value::as_i64)
        .unwrap_or(1);
    parent
        .get_mut("spec")
        .and_then(Value::as_object_mut)
        .ok_or_else(|| "the scaled object has no object-valued spec")?
        .insert("replicas".to_string(), Value::Number(replicas.into()));
    if previous != replicas {
        let generation = parent
            .pointer("/metadata/generation")
            .and_then(Value::as_i64)
            .unwrap_or(0);
        set_metadata_field(
            &mut parent,
            "generation",
            Value::Number((generation + 1).into()),
        );
    }
    Ok(parent)
}

fn apply_scale_patch(
    kind_of_patch: PatchKind,
    existing: &Value,
    patch_doc: &Value,
) -> Result<Value, String> {
    match kind_of_patch {
        PatchKind::Json => {
            let mut value = existing.clone();
            crate::patch::json_patch::apply(&mut value, patch_doc)
                .map_err(|_| "the submitted JSON Patch could not be applied".to_string())?;
            Ok(value)
        }
        PatchKind::Merge => {
            let mut value = existing.clone();
            crate::patch::merge_patch::apply(&mut value, patch_doc);
            Ok(value)
        }
        PatchKind::StrategicMerge => {
            let schema = protobuf::schema_for_gvk("autoscaling", "v1", "Scale")
                .ok_or_else(|| "autoscaling/v1 Scale has no known schema".to_string())?;
            Ok(crate::patch::strategic_merge::apply(
                schema, existing, patch_doc,
            ))
        }
    }
}

fn scale_selector(parent: &Value) -> Option<String> {
    let status_selector = parent
        .pointer("/status/selector")
        .and_then(Value::as_str)
        .filter(|selector| !selector.is_empty());
    if status_selector.is_some() {
        return status_selector.map(str::to_string);
    }

    let selector = parent.pointer("/spec/selector")?.as_object()?;
    let mut requirements = Vec::new();
    if let Some(labels) = selector.get("matchLabels").and_then(Value::as_object) {
        let mut entries = labels.iter().collect::<Vec<_>>();
        entries.sort_by(|(left, _), (right, _)| left.cmp(right));
        requirements.extend(
            entries
                .into_iter()
                .filter_map(|(key, value)| value.as_str().map(|value| format!("{key}={value}"))),
        );
    }
    if let Some(expressions) = selector.get("matchExpressions").and_then(Value::as_array) {
        for expression in expressions {
            let Some(key) = expression.get("key").and_then(Value::as_str) else {
                continue;
            };
            let operator = expression
                .get("operator")
                .and_then(Value::as_str)
                .unwrap_or("");
            let values = expression
                .get("values")
                .and_then(Value::as_array)
                .map(|values| values.iter().filter_map(Value::as_str).collect::<Vec<_>>())
                .unwrap_or_default();
            let rendered = match operator {
                "In" => format!("{key} in ({})", values.join(",")),
                "NotIn" => format!("{key} notin ({})", values.join(",")),
                "Exists" => key.to_string(),
                "DoesNotExist" => format!("!{key}"),
                _ => continue,
            };
            requirements.push(rendered);
        }
    }
    (!requirements.is_empty()).then(|| requirements.join(","))
}
