/// Replaces an existing object. `namespace: None` for a cluster-scoped
/// resource, same convention as [`get`]/[`create`]. Real optimistic
/// concurrency: reads the current object first, requires the submitted
/// body's own `metadata.resourceVersion` to match what's actually
/// stored, and writes with a `Txn` compared against that same revision
/// — a concurrent write between the read and this write loses the race
/// and gets a real `Conflict`, not a silent overwrite.
/// `metadata.creationTimestamp`/`uid` are preserved from the existing
/// object regardless of what the client submitted — real upstream
/// treats both as immutable after creation.
pub async fn update(
    storage: &mut StorageClient,
    group: &str,
    version: &str,
    resource: &str,
    namespace: Option<&str>,
    name: &str,
    body: &Value,
) -> Result<UpdateOutcome, Error> {
    update_with_options_and_manager(
        storage, group, version, resource, namespace, name, body, false, None,
    )
    .await
}

/// [`update`] with the real Kubernetes `dryRun=All` write option. The
/// candidate is prepared exactly like a normal update, but the final
/// optimistic-concurrency write is omitted.
pub async fn update_with_options(
    storage: &mut StorageClient,
    group: &str,
    version: &str,
    resource: &str,
    namespace: Option<&str>,
    name: &str,
    body: &Value,
    dry_run: bool,
) -> Result<UpdateOutcome, Error> {
    update_with_options_and_manager(
        storage, group, version, resource, namespace, name, body, dry_run, None,
    )
    .await
}

/// [`update_with_options`] with the request's field manager. Ordinary
/// updates use the same `Update` managed-fields operation as upstream and do
/// not report ownership conflicts; changed fields move to this manager.
pub async fn update_with_options_and_manager(
    storage: &mut StorageClient,
    group: &str,
    version: &str,
    resource: &str,
    namespace: Option<&str>,
    name: &str,
    body: &Value,
    dry_run: bool,
    field_manager: Option<&str>,
) -> Result<UpdateOutcome, Error> {
    let Some(resolved) = resolve_resource(storage, group, version, resource).await? else {
        return Ok(UpdateOutcome::UnknownResource);
    };
    let kind = resolved.kind.clone();

    let key = keys::object_key(group, resource, namespace, name);
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
        group,
        resource,
        &existing_kv.key,
        &existing_kv.value,
        existing_kv.mod_revision,
    )
    .await?;

    if let (Some(ns), Some(body_ns)) = (
        namespace,
        body.pointer("/metadata/namespace").and_then(Value::as_str),
    ) {
        if !body_ns.is_empty() && body_ns != ns {
            return Ok(UpdateOutcome::NamespaceMismatch);
        }
    }

    // Compared numerically, not as strings — resourceVersion is an
    // opaque string to a real client, but this build's own encoding of
    // it is always the decimal MVCC revision, so parsing avoids any
    // formatting-mismatch false negative (leading zeros, etc.).
    let Some(submitted_rv) = body
        .pointer("/metadata/resourceVersion")
        .and_then(Value::as_str)
        .and_then(|s| s.parse::<i64>().ok())
    else {
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
            let mut v: Vec<String> = validation::validate_required(schema, body)
                .into_iter()
                .map(|m| format!("{}: Required value", m.path))
                .collect();
            v.extend(
                validation::validate_types(schema, body)
                    .into_iter()
                    .map(|t| {
                        format!(
                            "{}: expected type {}, got {}",
                            t.path, t.expected, t.actual_kind
                        )
                    }),
            );
            v.extend(validation::validate_openapi_constraints(
                group, version, &kind, body,
            ));
            v
        }
        // Group K: same scope `create`'s own CRD branch runs.
        (None, Some(open_api_schema)) => {
            let mut v: Vec<String> =
                apiextensions::schema_validation::validate_required(open_api_schema, body)
                    .into_iter()
                    .map(|m| format!("{}: Required value", m.path))
                    .collect();
            v.extend(
                apiextensions::schema_validation::validate_types(open_api_schema, body)
                    .into_iter()
                    .map(|t| {
                        format!(
                            "{}: expected type {}, got {}",
                            t.path, t.expected, t.actual_kind
                        )
                    }),
            );
            v.extend(apiextensions::schema_validation::validate_constraints(
                open_api_schema,
                body,
            ));
            v
        }
        (None, None) => Vec::new(),
    };
    violations.extend(
        name_format_violations(group, resource, name)
            .into_iter()
            .map(|e| format!("metadata.name: {e}")),
    );
    violations.extend(metadata_format_violations(body));
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
        (None, Some(open_api_schema)) => {
            apiextensions::schema_defaults::apply_defaults(open_api_schema, body)
        }
        (None, None) => body.clone(),
    };
    let object = defaulting::apply_builtin_defaults(group, version, &kind, object);

    // CEL Phase 4: same real rule evaluation `create`'s own CRD branch
    // runs, `old_value: Some(&existing_object)` this time — real
    // upstream's own `oldSelf` binding is exactly the object as it was
    // immediately before this update.
    if let Some(open_api_schema) = &resolved.open_api_schema {
        let rule_violations = apiextensions::cel_evaluate::validate_object(
            open_api_schema,
            &object,
            Some(&existing_object),
        );
        if !rule_violations.is_empty() {
            return Ok(UpdateOutcome::Invalid(
                rule_violations.into_iter().map(|v| v.to_string()).collect(),
            ));
        }
    }

    persist_update(
        storage,
        resolved.schema,
        resolved.open_api_schema.as_ref(),
        resolved.storage_open_api_schema.as_ref(),
        &kind,
        group,
        version,
        resource,
        key,
        &existing_kv,
        &existing_object,
        namespace,
        object,
        dry_run,
        resolved.conversion_webhook.clone(),
        field_manager,
        "",
        resolved.has_status_subresource,
        false,
    )
    .await
}

/// Real upstream's generic status subresource (`GenericStatusREST`,
/// `k8s.io/apiserver/pkg/registry/generic/registry/store.go`'s own
/// `StatusREST`): a `PUT` through `<resource>/status` only ever changes
/// the object's `status` field — every other top-level field on the
/// submitted body (`spec`, most of `metadata`) is ignored, the existing
/// object's own spec/metadata survives untouched apart from the same
/// `creationTimestamp`/`uid` immutability [`persist_update`] already
/// enforces for a plain `update`. Same real optimistic concurrency as
/// `update` (submitted `metadata.resourceVersion` must match).
///
/// For a CRD-defined resource, the matched version's `status` schema is
/// applied to the replacement status: unknown fields are pruned and the
/// schema's required/type/local constraints are validated, just as for the
/// main resource. Built-in status strategies remain the generic, untyped
/// path because their per-kind status rules are hand-written upstream and
/// are not represented by this crate's generic discovery table. The
/// namespace-mismatch check `update` runs against the body is skipped (moot
/// here — the body's own `metadata`/`spec` are never read for anything but
/// `resourceVersion`). [`patch_status`] is this function's `PATCH` counterpart.
///
/// A CRD-defined resource whose matched version never declared
/// `subresources.status` has no `status` subresource at all — real
/// upstream doesn't even install this route for such a version — so
/// this returns `UnknownResource` (a real `404`) rather than silently
/// serving a status write real upstream itself would refuse. Every
/// built-in resource this crate resolves through the static table is
/// unaffected: `resolve_resource` always reports `true` for one, the
/// same "not modeled per-type yet" scope this crate's own discovery
/// already has for built-in subresources generally.
/// `dry_run` validates and returns the status candidate without persisting it.
/// Real upstream's `NamespaceFinalize` subresource: replaces
/// `spec.finalizers` only, exactly the way [`update_status`] replaces
/// `.status` only -- namespace-controller's own `finalize_namespace()`
/// is the intended (only, in practice) caller, removing the
/// `"kubernetes"` finalizer once a namespace's contents are fully
/// deleted. Issue #541/#559/#560's own follow-up: those fixes correctly
/// got a Namespace's deletion to defer and its contents to actually get
/// cleaned up, but this subresource had no handler wired to it at all
/// (`server/path.rs`'s `NAMESPACE_SUBRESOURCES` already listed
/// `"finalize"`, so requests routed here, they just all 404'd) -- so the
/// finalizer could never actually be removed and the namespace sat
/// "Terminating" forever. No CRD/status-schema validation here (unlike
/// `update_status`): `spec.finalizers` is a plain string array with
/// nothing upstream ever validates against a schema for this
/// subresource specifically.
pub async fn update_finalize(
    storage: &mut StorageClient,
    group: &str,
    version: &str,
    resource: &str,
    namespace: Option<&str>,
    name: &str,
    body: &Value,
    dry_run: bool,
) -> Result<UpdateOutcome, Error> {
    let Some(resolved) = resolve_resource(storage, group, version, resource).await? else {
        return Ok(UpdateOutcome::UnknownResource);
    };
    let kind = resolved.kind.clone();
    let key = keys::object_key(group, resource, namespace, name);
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
        group,
        resource,
        &existing_kv.key,
        &existing_kv.value,
        existing_kv.mod_revision,
    )
    .await?;
    let existing_object_for_request = convert_to_requested_version(
        storage,
        group,
        version,
        &kind,
        resolved.conversion_webhook.as_ref(),
        existing_object.clone(),
    )
    .await?;

    let Some(submitted_rv) = body
        .pointer("/metadata/resourceVersion")
        .and_then(Value::as_str)
        .and_then(|s| s.parse::<i64>().ok())
    else {
        return Ok(UpdateOutcome::MissingResourceVersion);
    };
    if submitted_rv != existing_kv.mod_revision {
        return Ok(UpdateOutcome::Conflict);
    }

    let mut object = existing_object_for_request;
    let finalizers = body
        .pointer("/spec/finalizers")
        .cloned()
        .unwrap_or_else(|| Value::Array(Vec::new()));
    if let Some(spec) = object.get_mut("spec").and_then(Value::as_object_mut) {
        spec.insert("finalizers".to_string(), finalizers);
    }

    persist_update(
        storage,
        resolved.schema,
        resolved.open_api_schema.as_ref(),
        resolved.storage_open_api_schema.as_ref(),
        &kind,
        group,
        version,
        resource,
        key,
        &existing_kv,
        &existing_object,
        namespace,
        object,
        dry_run,
        resolved.conversion_webhook.clone(),
        None,
        "finalize",
        false,
        false,
    )
    .await
}

pub async fn update_status(
    storage: &mut StorageClient,
    group: &str,
    version: &str,
    resource: &str,
    namespace: Option<&str>,
    name: &str,
    body: &Value,
    dry_run: bool,
) -> Result<UpdateOutcome, Error> {
    update_status_with_manager(
        storage, group, version, resource, namespace, name, body, dry_run, None,
    )
    .await
}

/// [`update_status`] with the request's field manager. Status writes use a
/// separate managed-fields subresource entry, as in upstream.
pub async fn update_status_with_manager(
    storage: &mut StorageClient,
    group: &str,
    version: &str,
    resource: &str,
    namespace: Option<&str>,
    name: &str,
    body: &Value,
    dry_run: bool,
    field_manager: Option<&str>,
) -> Result<UpdateOutcome, Error> {
    let Some(resolved) = resolve_resource(storage, group, version, resource).await? else {
        return Ok(UpdateOutcome::UnknownResource);
    };
    if !resolved.has_status_subresource {
        return Ok(UpdateOutcome::UnknownResource);
    }
    let kind = resolved.kind.clone();

    let key = keys::object_key(group, resource, namespace, name);
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
        group,
        resource,
        &existing_kv.key,
        &existing_kv.value,
        existing_kv.mod_revision,
    )
    .await?;
    let existing_object_for_request = convert_to_requested_version(
        storage,
        group,
        version,
        &kind,
        resolved.conversion_webhook.as_ref(),
        existing_object.clone(),
    )
    .await?;

    let Some(submitted_rv) = body
        .pointer("/metadata/resourceVersion")
        .and_then(Value::as_str)
        .and_then(|s| s.parse::<i64>().ok())
    else {
        return Ok(UpdateOutcome::MissingResourceVersion);
    };
    if submitted_rv != existing_kv.mod_revision {
        return Ok(UpdateOutcome::Conflict);
    }

    let mut object = existing_object_for_request;
    match body.get("status") {
        Some(status) => object["status"] = status.clone(),
        None => {
            if let Some(map) = object.as_object_mut() {
                map.remove("status");
            }
        }
    }

    let violations = validate_crd_status(&resolved.open_api_schema, &mut object);
    if !violations.is_empty() {
        return Ok(UpdateOutcome::Invalid(violations));
    }

    persist_update(
        storage,
        resolved.schema,
        resolved.open_api_schema.as_ref(),
        resolved.storage_open_api_schema.as_ref(),
        &kind,
        group,
        version,
        resource,
        key,
        &existing_kv,
        &existing_object,
        namespace,
        object,
        dry_run,
        resolved.conversion_webhook.clone(),
        field_manager,
        "status",
        false,
        false,
    )
    .await
}

/// Applies a CRD version's `properties.status` schema to a status-subresource
/// candidate. Wrapping the status value in a synthetic top-level object lets
/// the existing schema walkers report the correct `status.foo` paths and
/// check the status root's own type without duplicating their logic. The
/// returned value is already pruned; it is only written when validation
/// succeeds.
fn validate_crd_status(open_api_schema: &Option<Value>, object: &mut Value) -> Vec<String> {
    let Some(schema) = open_api_schema.as_ref() else {
        return Vec::new();
    };
    let Some(status_schema) = schema.pointer("/properties/status") else {
        return Vec::new();
    };
    let Some(status) = object.get("status").cloned() else {
        return Vec::new();
    };

    let wrapper_schema = json!({
        "type": "object",
        "properties": {"status": status_schema}
    });
    let candidate = json!({"status": status});
    let pruned = apiextensions::schema_pruning::prune(&wrapper_schema, &candidate);
    let mut violations: Vec<String> =
        apiextensions::schema_validation::validate_required(&wrapper_schema, &pruned)
            .into_iter()
            .map(|violation| format!("{}: Required value", violation.path))
            .collect();
    violations.extend(
        apiextensions::schema_validation::validate_types(&wrapper_schema, &pruned)
            .into_iter()
            .map(|violation| {
                format!(
                    "{}: expected type {}, got {}",
                    violation.path, violation.expected, violation.actual_kind
                )
            }),
    );
    violations.extend(apiextensions::schema_validation::validate_constraints(
        &wrapper_schema,
        &pruned,
    ));
    if violations.is_empty() {
        if let Some(status) = pruned.get("status") {
            object["status"] = status.clone();
        }
    }
    violations
}

/// The tail [`update`] and [`patch`] share once each has its own
/// candidate object in hand (a defaulted submitted body for `update`, a
/// patch-applied one for `patch`): preserve `creationTimestamp`/`uid`
/// from the existing object (real upstream treats both as immutable
/// after creation, regardless of what the caller's patch/body touched),
/// stamp the namespace, then a real optimistic-concurrency `Txn`
/// compared against the exact revision both callers already read —
/// a concurrent write between that read and this write loses the race
/// and gets a real `Conflict`, not a silent overwrite.
async fn persist_update(
    storage: &mut StorageClient,
    schema: Option<&str>,
    open_api_schema: Option<&Value>,
    storage_open_api_schema: Option<&Value>,
    kind: &str,
    group: &str,
    version: &str,
    resource: &str,
    key: String,
    existing_kv: &mvccpb::KeyValue,
    existing_object: &Value,
    namespace: Option<&str>,
    mut object: Value,
    dry_run: bool,
    conversion_webhook: Option<apiextensions::registry::ConversionWebhook>,
    field_manager: Option<&str>,
    managed_subresource: &str,
    has_status_subresource: bool,
    managed_fields_reconciled: bool,
) -> Result<UpdateOutcome, Error> {
    let semantic_violations = validation::validate_builtin_update_semantics(
        group,
        version,
        kind,
        existing_object,
        &object,
    );
    if !semantic_violations.is_empty() {
        return Ok(UpdateOutcome::Invalid(semantic_violations));
    }

    for field in ["creationTimestamp", "uid"] {
        if let Some(existing_value) = existing_object
            .pointer(&format!("/metadata/{field}"))
            .cloned()
        {
            set_metadata_field(&mut object, field, existing_value);
        }
    }
    if let Some(ns) = namespace {
        set_metadata_field(&mut object, "namespace", Value::String(ns.to_string()));
    }

    object = crate::scheme::conversion::to_version(group, version, kind, object);
    if field_manager.is_some() {
        let existing_for_fields = convert_to_requested_version(
            storage,
            group,
            version,
            kind,
            conversion_webhook.as_ref(),
            existing_object.clone(),
        )
        .await?;
        object = reconcile_managed_fields(
            schema,
            open_api_schema,
            &existing_for_fields,
            object,
            field_manager,
            "Update",
            managed_subresource,
            group,
            version,
            has_status_subresource,
        );
    } else if !managed_fields_reconciled {
        preserve_managed_fields(existing_object, &mut object);
    }
    object =
        convert_to_storage_version(storage, group, version, conversion_webhook.as_ref(), object)
            .await?;
    object = match revalidate_storage_object(storage_open_api_schema, object) {
        Ok(object) => object,
        Err(violations) => return Ok(UpdateOutcome::Invalid(violations)),
    };

    // Removing the last finalizer from an object already marked for deletion
    // completes the deletion. This mirrors the generic registry's
    // ShouldDeleteDuringUpdate path: the update is accepted, but the object
    // is removed atomically instead of being written back as a live object.
    if managed_subresource.is_empty()
        && has_deletion_timestamp(existing_object)
        && !has_finalizers(&object)
    {
        if dry_run {
            let object = convert_to_requested_version(
                storage,
                group,
                version,
                kind,
                conversion_webhook.as_ref(),
                object,
            )
            .await?;
            return Ok(UpdateOutcome::Updated(object));
        }
        let compare = pb::Compare {
            key: key.clone().into_bytes(),
            result: pb::compare::CompareResult::Equal as i32,
            target: pb::compare::CompareTarget::Mod as i32,
            target_union: Some(pb::compare::TargetUnion::ModRevision(
                existing_kv.mod_revision,
            )),
            range_end: Vec::new(),
        };
        let txn = pb::TxnRequest {
            compare: vec![compare],
            success: vec![pb::RequestOp {
                request: Some(pb::request_op::Request::RequestDeleteRange(
                    pb::DeleteRangeRequest {
                        key: key.into_bytes(),
                        prev_kv: true,
                        ..Default::default()
                    },
                )),
            }],
            failure: vec![],
        };
        let response = storage.txn(txn).await?;
        if !response.succeeded {
            return Ok(UpdateOutcome::Conflict);
        }
        let revision = response.header.map(|header| header.revision).unwrap_or(0);
        set_metadata_field(
            &mut object,
            "resourceVersion",
            Value::String(revision.to_string()),
        );
        let object = convert_to_requested_version(
            storage,
            group,
            version,
            kind,
            conversion_webhook.as_ref(),
            object,
        )
        .await?;
        return Ok(UpdateOutcome::Updated(object));
    }

    if dry_run {
        let object = convert_to_requested_version(
            storage,
            group,
            version,
            kind,
            conversion_webhook.as_ref(),
            object,
        )
        .await?;
        return Ok(UpdateOutcome::Updated(object));
    }

    let stored_version = conversion_webhook
        .as_ref()
        .map_or(version, |conversion| conversion.storage_version.as_str());
    let api_version = if group.is_empty() {
        stored_version.to_string()
    } else {
        format!("{group}/{stored_version}")
    };
    let object_bytes = match schema {
        Some(schema) => protobuf::encode_message(schema, &object)?,
        None => serde_json::to_vec(&object).map_err(protobuf::Error::Json)?,
    };
    let envelope = protobuf::wrap_unknown(&api_version, kind, &object_bytes);

    let compare = pb::Compare {
        key: key.clone().into_bytes(),
        result: pb::compare::CompareResult::Equal as i32,
        target: pb::compare::CompareTarget::Mod as i32,
        target_union: Some(pb::compare::TargetUnion::ModRevision(
            existing_kv.mod_revision,
        )),
        range_end: Vec::new(),
    };
    let envelope = encrypt_for_storage(storage, group, resource, key.as_bytes(), &envelope)?;
    let put = pb::PutRequest {
        key: key.into_bytes(),
        value: envelope,
        ..Default::default()
    };
    let txn = pb::TxnRequest {
        compare: vec![compare],
        success: vec![pb::RequestOp {
            request: Some(pb::request_op::Request::RequestPut(put)),
        }],
        failure: vec![],
    };
    let resp = storage.txn(txn).await?;
    if !resp.succeeded {
        // Lost the race: something else wrote to this key between our
        // read above and this write.
        return Ok(UpdateOutcome::Conflict);
    }

    let revision = resp.header.map(|h| h.revision).unwrap_or(0);
    set_metadata_field(
        &mut object,
        "resourceVersion",
        Value::String(revision.to_string()),
    );
    let object = convert_to_requested_version(
        storage,
        group,
        version,
        kind,
        conversion_webhook.as_ref(),
        object,
    )
    .await?;
    Ok(UpdateOutcome::Updated(object))
}
