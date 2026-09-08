/// The "prepare" half of [`server_side_apply`]: resolves the resource,
/// reads the current object, applies ownership, and validates/defaults the
/// candidate without persisting it. The listener runs admission against
/// this candidate before the transaction, as with [`patch_prepare`].
pub async fn apply_prepare(
    storage: &mut StorageClient,
    group: &str,
    version: &str,
    resource: &str,
    namespace: Option<&str>,
    name: &str,
    manager: &str,
    force: bool,
    config: &Value,
) -> Result<ApplyPrepareOutcome, Error> {
    let Some(resolved) = resolve_resource(storage, group, version, resource).await? else {
        return Ok(ApplyPrepareOutcome::UnknownResource);
    };
    let schema = resolved.schema;
    let open_api_schema = resolved.open_api_schema.clone();
    let ignored_fields = ignored_managed_fields(resolved.has_status_subresource, "");
    // Prune a CRD's apply configuration before field ownership is
    // calculated, so unknown fields cannot become owned. Prune the merged
    // candidate again before validation/defaulting, matching the ordering of
    // the ordinary CRD write paths.
    let effective_config = prune_runtime_schema(open_api_schema.as_ref(), config.clone());

    let key = keys::object_key(group, resource, namespace, name);
    let existing_resp = storage
        .range(RangeRequest {
            key: key.clone().into_bytes(),
            ..Default::default()
        })
        .await?;
    let api_version = if group.is_empty() {
        version.to_string()
    } else {
        format!("{group}/{version}")
    };

    let Some(existing_kv) = existing_resp.kvs.into_iter().next() else {
        // Create-on-apply: real upstream's own Apply can create a
        // brand-new object when none exists yet (`liveObject` starts
        // empty). Built-ins use the compiled schema; CRDs use their
        // established version's runtime OpenAPI schema.
        let live = json!({});
        let no_prior_managers = std::collections::BTreeMap::new();
        let applied_result = match (schema, open_api_schema.as_ref()) {
            (Some(schema), _) => crate::patch::updater::apply_with_ignored_fields(
                schema,
                &live,
                &effective_config,
                &no_prior_managers,
                manager,
                force,
                Some(&ignored_fields),
            ),
            (None, Some(schema)) => crate::patch::crd_apply::apply_with_ignored_fields(
                schema,
                &live,
                &effective_config,
                &no_prior_managers,
                manager,
                force,
                Some(&ignored_fields),
            ),
            (None, None) => return Ok(ApplyPrepareOutcome::UnsupportedForCrd),
        };
        let applied = match applied_result {
            Ok(a) => a,
            Err(conflicts) => return Ok(ApplyPrepareOutcome::Conflict(conflicts)),
        };
        let Some(candidate) = applied.object else {
            // The apply configuration was itself empty (merges to `{}`)
            // -- nothing real to create.
            return Ok(ApplyPrepareOutcome::NoOp(live));
        };
        let request_fields = match (schema, open_api_schema.as_ref()) {
            (Some(schema), _) => crate::patch::fieldset::set_from_object(schema, &effective_config)
                .recursive_difference(&ignored_fields),
            (None, Some(schema)) => {
                crate::patch::crd_apply::set_from_object(schema, &effective_config)
                    .recursive_difference(&ignored_fields)
            }
            (None, None) => return Ok(ApplyPrepareOutcome::UnsupportedForCrd),
        };
        let applied = crate::patch::updater::reconcile_versioned_apply_with_ignored_fields(
            &live,
            &candidate,
            &crate::patch::managed_fields::VersionedManagers::new(),
            manager,
            &api_version,
            request_fields,
            &BTreeMap::new(),
            force,
            Some(&ignored_fields),
        )
        .expect("an empty managed-fields map cannot conflict");
        let Some(mut object) = applied.object else {
            return Ok(ApplyPrepareOutcome::NoOp(live));
        };

        set_metadata_field(
            &mut object,
            "creationTimestamp",
            Value::String(now_rfc3339()),
        );
        set_metadata_field(
            &mut object,
            "uid",
            Value::String(uuid::Uuid::new_v4().to_string()),
        );
        // The object's identity comes from the URL, same as every other
        // verb here (`persist_update` forces `namespace` from the URL
        // the same unconditional way) -- not from whatever `config`'s
        // own body happened to say.
        set_metadata_field(&mut object, "name", Value::String(name.to_string()));
        if let Some(ns) = namespace {
            set_metadata_field(&mut object, "namespace", Value::String(ns.to_string()));
        }
        let rebuilt = crate::patch::managed_fields::rebuild_versioned_managed_fields(
            &[],
            &applied.managers,
            manager,
            "",
            "Apply",
            &api_version,
            Some(&now_rfc3339()),
        );
        set_metadata_field(
            &mut object,
            "managedFields",
            crate::patch::managed_fields::render_managed_fields(&rebuilt),
        );
        let object = prune_runtime_schema(open_api_schema.as_ref(), object);

        let mut violations: Vec<String> = match (schema, open_api_schema.as_ref()) {
            (Some(schema), _) => {
                let mut violations = validation::validate_required(schema, &object)
                    .into_iter()
                    .map(|m| format!("{}: Required value", m.path))
                    .collect::<Vec<_>>();
                violations.extend(validation::validate_openapi_constraints(
                    group,
                    version,
                    &resolved.kind,
                    &object,
                ));
                violations
            }
            (None, Some(schema)) => {
                let mut violations: Vec<String> =
                    apiextensions::schema_validation::validate_required(schema, &object)
                        .into_iter()
                        .map(|m| format!("{}: Required value", m.path))
                        .collect();
                violations.extend(apiextensions::schema_validation::validate_constraints(
                    schema, &object,
                ));
                violations
            }
            (None, None) => Vec::new(),
        };
        match (schema, open_api_schema.as_ref()) {
            (Some(schema), _) => {
                violations.extend(validation::validate_types(schema, &object).into_iter().map(
                    |t| {
                        format!(
                            "{}: expected type {}, got {}",
                            t.path, t.expected, t.actual_kind
                        )
                    },
                ))
            }
            (None, Some(schema)) => violations.extend(
                apiextensions::schema_validation::validate_types(schema, &object)
                    .into_iter()
                    .map(|t| {
                        format!(
                            "{}: expected type {}, got {}",
                            t.path, t.expected, t.actual_kind
                        )
                    }),
            ),
            (None, None) => {}
        }
        violations.extend(
            name_format_violations(group, resource, name)
                .into_iter()
                .map(|e| format!("metadata.name: {e}")),
        );
        violations.extend(metadata_format_violations(&object));
        if !violations.is_empty() {
            return Ok(ApplyPrepareOutcome::Invalid(violations));
        }
        let object = match (schema, open_api_schema.as_ref()) {
            (Some(schema), _) => defaulting::apply_defaults(schema, &object),
            (None, Some(schema)) => apiextensions::schema_defaults::apply_defaults(schema, &object),
            (None, None) => object,
        };
        let object = defaulting::apply_builtin_defaults(group, version, &resolved.kind, object);
        if let Some(schema) = &open_api_schema {
            let rule_violations =
                apiextensions::cel_evaluate::validate_object(schema, &object, None);
            if !rule_violations.is_empty() {
                return Ok(ApplyPrepareOutcome::Invalid(
                    rule_violations.into_iter().map(|v| v.to_string()).collect(),
                ));
            }
        }

        return Ok(ApplyPrepareOutcome::Ready(
            object,
            ApplyContext {
                schema,
                storage_open_api_schema: resolved.storage_open_api_schema,
                kind: resolved.kind,
                conversion_webhook: resolved.conversion_webhook,
                has_status_subresource: resolved.has_status_subresource,
                key,
                existing: None,
            },
        ));
    };

    let live = decrypt_and_decode_with_rotation(
        storage,
        group,
        resource,
        &existing_kv.key,
        &existing_kv.value,
        existing_kv.mod_revision,
    )
    .await?;
    let live_for_request = convert_to_requested_version(
        storage,
        group,
        version,
        &resolved.kind,
        resolved.conversion_webhook.as_ref(),
        live.clone(),
    )
    .await?;

    // Managed-field paths are versioned data, not ordinary object fields.
    // Read them from the decoded storage object before projecting the object
    // through a different request version; the target OpenAPI projection can
    // legitimately omit the internal fieldsV1 shape.
    let stored_managed_fields = live
        .pointer("/metadata/managedFields")
        .cloned()
        .unwrap_or_else(|| Value::Array(Vec::new()));
    // A stored `managedFields` this crate can't parse (malformed, or an
    // entry with a `fieldsType` this crate doesn't understand — see
    // `managed_fields::parse_managed_fields`'s own doc comment) degrades
    // to "no prior bookkeeping" rather than failing the whole apply: the
    // object itself is still perfectly real and applicable, only the
    // ownership history is unrecoverable.
    let entries = crate::patch::managed_fields::parse_managed_fields(&stored_managed_fields)
        .unwrap_or_default();
    let managers = crate::patch::managed_fields::to_versioned_managers_map(&entries, &api_version);
    let managers = reconcile_versioned_managers_with_schema(
        storage,
        group,
        &api_version,
        open_api_schema.as_ref(),
        schema,
        resource,
        &resolved.kind,
        &managers,
    )
    .await?;
    // The existing single-schema updater remains the request-version merge
    // and prune implementation. Managers recorded under another version are
    // intentionally withheld from it; their ownership is reconciled below
    // using comparisons made after conversion into their own schemas.
    let request_managers = managers_for_request(&managers, &api_version);

    let applied_result = match (schema, open_api_schema.as_ref()) {
        (Some(schema), _) => crate::patch::updater::apply_with_ignored_fields(
            schema,
            &live_for_request,
            &effective_config,
            &request_managers,
            manager,
            force,
            Some(&ignored_fields),
        ),
        (None, Some(schema)) => crate::patch::crd_apply::apply_with_ignored_fields(
            schema,
            &live_for_request,
            &effective_config,
            &request_managers,
            manager,
            force,
            Some(&ignored_fields),
        ),
        (None, None) => return Ok(ApplyPrepareOutcome::UnsupportedForCrd),
    };
    let applied = match applied_result {
        Ok(a) => a,
        Err(conflicts) => return Ok(ApplyPrepareOutcome::Conflict(conflicts)),
    };

    let candidate = match applied.object {
        Some(candidate) => candidate,
        None => {
            // A request-version no-op can still be a real Apply: the
            // applying manager may be claiming these fields for the first
            // time, or may be changing ownership without changing any
            // values. Keep the live candidate so the versioned
            // prune/reconciliation phase can decide whether the managed
            // fields bookkeeping itself needs to be persisted.
            live_for_request.clone()
        }
    };

    let request_fields = match (schema, open_api_schema.as_ref()) {
        (Some(schema), _) => crate::patch::fieldset::set_from_object(schema, &effective_config)
            .recursive_difference(&ignored_fields),
        (None, Some(schema)) => crate::patch::crd_apply::set_from_object(schema, &effective_config)
            .recursive_difference(&ignored_fields),
        (None, None) => return Ok(ApplyPrepareOutcome::UnsupportedForCrd),
    };
    let candidate = if let Some(last_state) = managers
        .get(manager)
        .filter(|state| state.api_version != api_version && !state.fields.is_empty())
    {
        prune_versioned_apply(
            storage,
            group,
            version,
            resource,
            &resolved.kind,
            schema,
            open_api_schema.as_ref(),
            resolved.conversion_webhook.as_ref(),
            candidate,
            &managers,
            manager,
            last_state,
            &request_fields,
        )
        .await?
    } else {
        candidate
    };

    let comparisons = compare_managed_fields_in_recorded_versions(
        storage,
        group,
        version,
        resource,
        &resolved.kind,
        schema,
        open_api_schema.as_ref(),
        resolved.conversion_webhook.as_ref(),
        &live,
        &live_for_request,
        &candidate,
        &entries,
    )
    .await?;
    let applied = match crate::patch::updater::reconcile_versioned_apply_with_ignored_fields(
        &live_for_request,
        &candidate,
        &managers,
        manager,
        &api_version,
        request_fields,
        &comparisons,
        force,
        Some(&ignored_fields),
    ) {
        Ok(applied) => applied,
        Err(conflicts) => return Ok(ApplyPrepareOutcome::Conflict(conflicts)),
    };
    let mut object = match applied.object {
        Some(object) => object,
        None if applied.managers == managers => {
            // Neither the object nor its ownership changed. This is the
            // genuine no-op case; an unchanged object with changed manager
            // state must continue through managedFields persistence below.
            return Ok(ApplyPrepareOutcome::NoOp(live_for_request));
        }
        None => live_for_request.clone(),
    };

    let rebuilt = crate::patch::managed_fields::rebuild_versioned_managed_fields(
        &entries,
        &applied.managers,
        manager,
        "",
        "Apply",
        &api_version,
        Some(&now_rfc3339()),
    );
    set_metadata_field(
        &mut object,
        "managedFields",
        crate::patch::managed_fields::render_managed_fields(&rebuilt),
    );
    let object = prune_runtime_schema(open_api_schema.as_ref(), object);

    let mut violations: Vec<String> = match (schema, open_api_schema.as_ref()) {
        (Some(schema), _) => {
            let mut violations = validation::validate_required(schema, &object)
                .into_iter()
                .map(|m| format!("{}: Required value", m.path))
                .collect::<Vec<_>>();
            violations.extend(validation::validate_openapi_constraints(
                group,
                version,
                &resolved.kind,
                &object,
            ));
            violations
        }
        (None, Some(schema)) => {
            let mut violations: Vec<String> =
                apiextensions::schema_validation::validate_required(schema, &object)
                    .into_iter()
                    .map(|m| format!("{}: Required value", m.path))
                    .collect();
            violations.extend(apiextensions::schema_validation::validate_constraints(
                schema, &object,
            ));
            violations
        }
        (None, None) => Vec::new(),
    };
    match (schema, open_api_schema.as_ref()) {
        (Some(schema), _) => violations.extend(
            validation::validate_types(schema, &object)
                .into_iter()
                .map(|t| {
                    format!(
                        "{}: expected type {}, got {}",
                        t.path, t.expected, t.actual_kind
                    )
                }),
        ),
        (None, Some(schema)) => violations.extend(
            apiextensions::schema_validation::validate_types(schema, &object)
                .into_iter()
                .map(|t| {
                    format!(
                        "{}: expected type {}, got {}",
                        t.path, t.expected, t.actual_kind
                    )
                }),
        ),
        (None, None) => {}
    }
    violations.extend(
        name_format_violations(group, resource, name)
            .into_iter()
            .map(|e| format!("metadata.name: {e}")),
    );
    violations.extend(metadata_format_violations(&object));
    if !violations.is_empty() {
        return Ok(ApplyPrepareOutcome::Invalid(violations));
    }
    let object = match (schema, open_api_schema.as_ref()) {
        (Some(schema), _) => defaulting::apply_defaults(schema, &object),
        (None, Some(schema)) => apiextensions::schema_defaults::apply_defaults(schema, &object),
        (None, None) => object,
    };
    let object = defaulting::apply_builtin_defaults(group, version, &resolved.kind, object);
    if let Some(schema) = &open_api_schema {
        let rule_violations =
            apiextensions::cel_evaluate::validate_object(schema, &object, Some(&live_for_request));
        if !rule_violations.is_empty() {
            return Ok(ApplyPrepareOutcome::Invalid(
                rule_violations.into_iter().map(|v| v.to_string()).collect(),
            ));
        }
    }

    Ok(ApplyPrepareOutcome::Ready(
        object,
        ApplyContext {
            schema,
            storage_open_api_schema: resolved.storage_open_api_schema,
            kind: resolved.kind,
            conversion_webhook: resolved.conversion_webhook,
            has_status_subresource: resolved.has_status_subresource,
            key,
            existing: Some((existing_kv, live)),
        },
    ))
}
