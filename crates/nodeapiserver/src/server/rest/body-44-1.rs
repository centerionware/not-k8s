    let Some(resolved) = resolve_resource(storage, group, version, resource).await? else {
        return Ok(UpdateOutcome::UnknownResource);
    };
    let kind = resolved.kind.clone();

    let key = keys::object_key(group, resource, namespace, name);
    let existing_resp = storage.range(RangeRequest { key: key.clone().into_bytes(), ..Default::default() }).await?;
    let Some(existing_kv) = existing_resp.kvs.into_iter().next() else {
        return Ok(UpdateOutcome::ObjectNotFound);
    };
    let existing_object = decrypt_and_decode_with_rotation(storage, group, resource, &existing_kv.key, &existing_kv.value, existing_kv.mod_revision).await?;

    if let (Some(ns), Some(body_ns)) = (namespace, body.pointer("/metadata/namespace").and_then(Value::as_str)) {
        if !body_ns.is_empty() && body_ns != ns {
            return Ok(UpdateOutcome::NamespaceMismatch);
        }
    }

    // Compared numerically, not as strings — resourceVersion is an
    // opaque string to a real client, but this build's own encoding of
    // it is always the decimal MVCC revision, so parsing avoids any
    // formatting-mismatch false negative (leading zeros, etc.).
    let Some(submitted_rv) = body.pointer("/metadata/resourceVersion").and_then(Value::as_str).and_then(|s| s.parse::<i64>().ok()) else {
        return Ok(UpdateOutcome::MissingResourceVersion);
    };
    if submitted_rv != existing_kv.mod_revision {
        return Ok(UpdateOutcome::Conflict);
    }

    // Group K: same pruning `create` runs, same order (before validation/
    // defaulting).
    let pruned_body;
    let body: &Value = match &resolved.open_api_schema {
        Some(open_api_schema) => {
            pruned_body = apiextensions::schema_pruning::prune(open_api_schema, body);
            &pruned_body
        }
        None => body,
    };

    let mut violations: Vec<String> = match (resolved.schema, &resolved.open_api_schema) {
        (Some(schema), _) => {
            let mut v: Vec<String> = validation::validate_required(schema, body).into_iter().map(|m| format!("{}: Required value", m.path)).collect();
            v.extend(validation::validate_types(schema, body).into_iter().map(|t| format!("{}: expected type {}, got {}", t.path, t.expected, t.actual_kind)));
            v
        }
        // Group K: same scope `create`'s own CRD branch runs.
        (None, Some(open_api_schema)) => {
            let mut v: Vec<String> = apiextensions::schema_validation::validate_required(open_api_schema, body).into_iter().map(|m| format!("{}: Required value", m.path)).collect();
            v.extend(apiextensions::schema_validation::validate_types(open_api_schema, body).into_iter().map(|t| format!("{}: expected type {}, got {}", t.path, t.expected, t.actual_kind)));
            v.extend(apiextensions::schema_validation::validate_constraints(open_api_schema, body));
            v
        }
        (None, None) => Vec::new(),
    };
    violations.extend(name_format_violations(group, resource, name).into_iter().map(|e| format!("metadata.name: {e}")));
    // Group K / CEL Phase 3: same real static cost check `create`'s own
    // CRD branch runs.
    if group == "apiextensions.k8s.io" && resource == "customresourcedefinitions" {
        violations.extend(apiextensions::cel_validations::validate_crd_cel_costs(body));
        violations.extend(apiextensions::cel_validations::validate_crd_cel_types(body));
    }
    if !violations.is_empty() {
        return Ok(UpdateOutcome::Invalid(violations));
    }

    let object = match (resolved.schema, &resolved.open_api_schema) {
        (Some(schema), _) => defaulting::apply_defaults(schema, body),
        (None, Some(open_api_schema)) => apiextensions::schema_defaults::apply_defaults(open_api_schema, body),
        (None, None) => body.clone(),
    };

    // CEL Phase 4: same real rule evaluation `create`'s own CRD branch
    // runs, `old_value: Some(&existing_object)` this time — real
    // upstream's own `oldSelf` binding is exactly the object as it was
    // immediately before this update.
    if let Some(open_api_schema) = &resolved.open_api_schema {
        let rule_violations = apiextensions::cel_evaluate::validate_object(open_api_schema, &object, Some(&existing_object));
        if !rule_violations.is_empty() {
            return Ok(UpdateOutcome::Invalid(rule_violations.into_iter().map(|v| v.to_string()).collect()));
        }
    }

