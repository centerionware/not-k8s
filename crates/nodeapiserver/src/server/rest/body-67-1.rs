    let Some(resolved) = resolve_resource(storage, group, version, resource).await? else {
        return Ok(ApplyPrepareOutcome::UnknownResource);
    };
    let schema = resolved.schema;
    let open_api_schema = resolved.open_api_schema.clone();
    // Prune a CRD's apply configuration before field ownership is
    // calculated, so unknown fields cannot become owned. Prune the merged
    // candidate again before validation/defaulting, matching the ordering of
    // the ordinary CRD write paths.
    let effective_config = prune_runtime_schema(open_api_schema.as_ref(), config.clone());

    let key = keys::object_key(group, resource, namespace, name);
    let existing_resp = storage.range(RangeRequest { key: key.clone().into_bytes(), ..Default::default() }).await?;
    let api_version = if group.is_empty() { version.to_string() } else { format!("{group}/{version}") };

    let Some(existing_kv) = existing_resp.kvs.into_iter().next() else {
        // Create-on-apply: real upstream's own Apply can create a
        // brand-new object when none exists yet (`liveObject` starts
        // empty). Built-ins use the compiled schema; CRDs use their
        // established version's runtime OpenAPI schema.
        let live = json!({});
        let no_prior_managers = std::collections::BTreeMap::new();
        let applied_result = match (schema, open_api_schema.as_ref()) {
            (Some(schema), _) => crate::patch::updater::apply(schema, &live, &effective_config, &no_prior_managers, manager, force),
            (None, Some(schema)) => crate::patch::crd_apply::apply(schema, &live, &effective_config, &no_prior_managers, manager, force),
            (None, None) => return Ok(ApplyPrepareOutcome::UnsupportedForCrd),
        };
        let applied = match applied_result {
            Ok(a) => a,
            Err(conflicts) => return Ok(ApplyPrepareOutcome::Conflict(conflicts)),
        };
        let Some(mut object) = applied.object else {
            // The apply configuration was itself empty (merges to `{}`)
            // -- nothing real to create.
            return Ok(ApplyPrepareOutcome::NoOp(live));
        };

        set_metadata_field(&mut object, "creationTimestamp", Value::String(now_rfc3339()));
        set_metadata_field(&mut object, "uid", Value::String(uuid::Uuid::new_v4().to_string()));
        // The object's identity comes from the URL, same as every other
        // verb here (`persist_update` forces `namespace` from the URL
        // the same unconditional way) -- not from whatever `config`'s
        // own body happened to say.
        set_metadata_field(&mut object, "name", Value::String(name.to_string()));
        if let Some(ns) = namespace {
            set_metadata_field(&mut object, "namespace", Value::String(ns.to_string()));
        }
        let rebuilt = crate::patch::managed_fields::rebuild_managed_fields(&[], &applied.managers, manager, "", "Apply", &api_version, Some(&now_rfc3339()));
        set_metadata_field(&mut object, "managedFields", crate::patch::managed_fields::render_managed_fields(&rebuilt));
        let object = prune_runtime_schema(open_api_schema.as_ref(), object);

        let mut violations: Vec<String> = match (schema, open_api_schema.as_ref()) {
            (Some(schema), _) => validation::validate_required(schema, &object).into_iter().map(|m| format!("{}: Required value", m.path)).collect(),
            (None, Some(schema)) => {
                let mut violations: Vec<String> = apiextensions::schema_validation::validate_required(schema, &object)
                    .into_iter()
                    .map(|m| format!("{}: Required value", m.path))
                    .collect();
                violations.extend(apiextensions::schema_validation::validate_constraints(schema, &object));
                violations
            }
            (None, None) => Vec::new(),
        };
        match (schema, open_api_schema.as_ref()) {
            (Some(schema), _) => violations.extend(validation::validate_types(schema, &object).into_iter().map(|t| format!("{}: expected type {}, got {}", t.path, t.expected, t.actual_kind))),
            (None, Some(schema)) => violations.extend(apiextensions::schema_validation::validate_types(schema, &object).into_iter().map(|t| format!("{}: expected type {}, got {}", t.path, t.expected, t.actual_kind))),
            (None, None) => {}
        }
        violations.extend(name_format_violations(group, resource, name).into_iter().map(|e| format!("metadata.name: {e}")));
        if !violations.is_empty() {
            return Ok(ApplyPrepareOutcome::Invalid(violations));
        }
        let object = match (schema, open_api_schema.as_ref()) {
            (Some(schema), _) => defaulting::apply_defaults(schema, &object),
            (None, Some(schema)) => apiextensions::schema_defaults::apply_defaults(schema, &object),
            (None, None) => object,
        };
        if let Some(schema) = &open_api_schema {
            let rule_violations = apiextensions::cel_evaluate::validate_object(schema, &object, None);
            if !rule_violations.is_empty() {
                return Ok(ApplyPrepareOutcome::Invalid(rule_violations.into_iter().map(|v| v.to_string()).collect()));
            }
        }

        return Ok(ApplyPrepareOutcome::Ready(object, ApplyContext { schema, storage_open_api_schema: resolved.storage_open_api_schema, kind: resolved.kind, conversion_webhook: resolved.conversion_webhook, key, existing: None }));
    };

    let live = decrypt_and_decode_with_rotation(storage, group, resource, &existing_kv.key, &existing_kv.value, existing_kv.mod_revision).await?;
    let live_for_request = convert_to_requested_version(storage, group, version, &resolved.kind, resolved.conversion_webhook.as_ref(), live.clone()).await?;

    let stored_managed_fields = live_for_request.pointer("/metadata/managedFields").cloned().unwrap_or_else(|| Value::Array(Vec::new()));
    // A stored `managedFields` this crate can't parse (malformed, or an
    // entry with a `fieldsType` this crate doesn't understand — see
    // `managed_fields::parse_managed_fields`'s own doc comment) degrades
    // to "no prior bookkeeping" rather than failing the whole apply: the
    // object itself is still perfectly real and applicable, only the
    // ownership history is unrecoverable.
    let entries = crate::patch::managed_fields::parse_managed_fields(&stored_managed_fields).unwrap_or_default();
    let managers = crate::patch::managed_fields::to_managers_map(&entries);

    let applied_result = match (schema, open_api_schema.as_ref()) {
        (Some(schema), _) => crate::patch::updater::apply(schema, &live_for_request, &effective_config, &managers, manager, force),
        (None, Some(schema)) => crate::patch::crd_apply::apply(schema, &live_for_request, &effective_config, &managers, manager, force),
        (None, None) => return Ok(ApplyPrepareOutcome::UnsupportedForCrd),
    };
    let applied = match applied_result {
        Ok(a) => a,
        Err(conflicts) => return Ok(ApplyPrepareOutcome::Conflict(conflicts)),
    };

    let Some(mut object) = applied.object else {
        let live = convert_to_requested_version(storage, group, version, &resolved.kind, resolved.conversion_webhook.as_ref(), live).await?;
        return Ok(ApplyPrepareOutcome::NoOp(live));
    };

    let rebuilt = crate::patch::managed_fields::rebuild_managed_fields(&entries, &applied.managers, manager, "", "Apply", &api_version, Some(&now_rfc3339()));
    set_metadata_field(&mut object, "managedFields", crate::patch::managed_fields::render_managed_fields(&rebuilt));
    let object = prune_runtime_schema(open_api_schema.as_ref(), object);

    let mut violations: Vec<String> = match (schema, open_api_schema.as_ref()) {
        (Some(schema), _) => validation::validate_required(schema, &object).into_iter().map(|m| format!("{}: Required value", m.path)).collect(),
        (None, Some(schema)) => {
            let mut violations: Vec<String> = apiextensions::schema_validation::validate_required(schema, &object)
                .into_iter()
                .map(|m| format!("{}: Required value", m.path))
                .collect();
            violations.extend(apiextensions::schema_validation::validate_constraints(schema, &object));
            violations
        }
        (None, None) => Vec::new(),
    };
    match (schema, open_api_schema.as_ref()) {
        (Some(schema), _) => violations.extend(validation::validate_types(schema, &object).into_iter().map(|t| format!("{}: expected type {}, got {}", t.path, t.expected, t.actual_kind))),
        (None, Some(schema)) => violations.extend(apiextensions::schema_validation::validate_types(schema, &object).into_iter().map(|t| format!("{}: expected type {}, got {}", t.path, t.expected, t.actual_kind))),
        (None, None) => {}
    }
    violations.extend(name_format_violations(group, resource, name).into_iter().map(|e| format!("metadata.name: {e}")));
    if !violations.is_empty() {
        return Ok(ApplyPrepareOutcome::Invalid(violations));
    }
    let object = match (schema, open_api_schema.as_ref()) {
        (Some(schema), _) => defaulting::apply_defaults(schema, &object),
        (None, Some(schema)) => apiextensions::schema_defaults::apply_defaults(schema, &object),
        (None, None) => object,
    };
    if let Some(schema) = &open_api_schema {
        let rule_violations = apiextensions::cel_evaluate::validate_object(schema, &object, Some(&live_for_request));
        if !rule_violations.is_empty() {
            return Ok(ApplyPrepareOutcome::Invalid(rule_violations.into_iter().map(|v| v.to_string()).collect()));
        }
    }

