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
    let existing_object = decrypt_and_decode(storage, "", "pods", &existing_kv.key, &existing_kv.value)?;

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
    let Some(target_name) = body.pointer("/target/name").and_then(Value::as_str).filter(|name| !name.is_empty()) else {
        violations.push("target.name: Required value".to_string());
        return Ok(BindOutcome::Invalid(violations));
    };
    if let Some(uid) = body.pointer("/metadata/uid").and_then(Value::as_str) {
        if existing_object.pointer("/metadata/uid").and_then(Value::as_str) != Some(uid) {
            return Ok(BindOutcome::Conflict);
        }
    }
    if let Some(resource_version) = body.pointer("/metadata/resourceVersion").and_then(Value::as_str) {
        if resource_version.parse::<i64>().ok() != Some(existing_kv.mod_revision) {
            return Ok(BindOutcome::Conflict);
        }
    }
    if existing_object.pointer("/metadata/deletionTimestamp").is_some_and(|timestamp| !timestamp.is_null()) {
        return Ok(BindOutcome::Conflict);
    }
    if existing_object.pointer("/spec/nodeName").and_then(Value::as_str).is_some_and(|node| !node.is_empty()) {
        return Ok(BindOutcome::Conflict);
    }
    if existing_object.pointer("/spec/schedulingGates").and_then(Value::as_array).is_some_and(|gates| !gates.is_empty()) {
        violations.push("spec.schedulingGates: Pod has scheduling gates".to_string());
    }
    if !violations.is_empty() {
        return Ok(BindOutcome::Invalid(violations));
    }

    let mut object = existing_object.clone();
    let Some(object_map) = object.as_object_mut() else {
        return Ok(BindOutcome::Invalid(vec!["Pod must be an object".to_string()]));
    };
    {
        let metadata = object_map.entry("metadata").or_insert_with(|| json!({}));
        let Some(metadata) = metadata.as_object_mut() else {
            return Ok(BindOutcome::Invalid(vec!["metadata must be an object".to_string()]));
        };
        for field in ["annotations", "labels"] {
            if let Some(values) = body.pointer(&format!("/metadata/{field}")) {
                let Some(values) = values.as_object() else {
                    return Ok(BindOutcome::Invalid(vec![format!("metadata.{field} must be an object")]));
                };
                let destination = metadata.entry(field).or_insert_with(|| json!({}));
                let Some(destination) = destination.as_object_mut() else {
                    return Ok(BindOutcome::Invalid(vec![format!("metadata.{field} must be an object")]));
                };
                for (key, value) in values {
                    destination.insert(key.clone(), value.clone());
                }
            }
        }
    }
    let spec = object_map.entry("spec").or_insert_with(|| json!({}));
    let Some(spec) = spec.as_object_mut() else {
        return Ok(BindOutcome::Invalid(vec!["spec must be an object".to_string()]));
    };
    spec.insert("nodeName".to_string(), Value::String(target_name.to_string()));

    let status = object_map.entry("status").or_insert_with(|| json!({}));
    let Some(status) = status.as_object_mut() else {
        return Ok(BindOutcome::Invalid(vec!["status must be an object".to_string()]));
    };
    let conditions = status.entry("conditions").or_insert_with(|| json!([]));
    let Some(conditions) = conditions.as_array_mut() else {
        return Ok(BindOutcome::Invalid(vec!["status.conditions must be an array".to_string()]));
    };
    let message = format!("Successfully assigned {namespace}/{name} to {target_name}");
    if let Some(condition) = conditions.iter_mut().find(|condition| condition.get("type").and_then(Value::as_str) == Some("PodScheduled")) {
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

