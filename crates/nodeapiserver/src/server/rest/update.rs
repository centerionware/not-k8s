#[derive(Debug, PartialEq)]
pub enum UpdateOutcome {
    Updated(Value),
    UnknownResource,
    /// No object exists at this key — this build doesn't support
    /// create-on-update (`AllowCreateOnUpdate`, real upstream's own
    /// opt-in a handful of types use), named honestly rather than
    /// silently creating one.
    ObjectNotFound,
    /// The submitted body had no `metadata.resourceVersion` at all —
    /// real upstream's own generic registry requires one for `PUT`
    /// (optimistic concurrency has nothing to compare against
    /// otherwise).
    MissingResourceVersion,
    /// The submitted `resourceVersion` didn't match what's currently
    /// stored — a real conflict, matching real upstream's own
    /// `errors.NewConflict`.
    Conflict,
    NamespaceMismatch,
    Invalid(Vec<String>),
    /// [`patch`] only: the `Content-Type` wasn't one of the three real
    /// patch media types this build understands.
    UnsupportedPatchType,
}

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
    update_with_options(
        storage, group, version, resource, namespace, name, body, false,
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
    let existing_object = decrypt_and_decode(
        storage,
        group,
        resource,
        &existing_kv.key,
        &existing_kv.value,
    )?;

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
    // Group K / CEL Phase 3: same real static cost check `create`'s own
    // CRD branch runs.
    if group == "apiextensions.k8s.io" && resource == "customresourcedefinitions" {
        violations.extend(apiextensions::cel_validations::validate_crd_cel_costs(body));
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
pub async fn update_status(
    storage: &mut StorageClient,
    group: &str,
    version: &str,
    resource: &str,
    namespace: Option<&str>,
    name: &str,
    body: &Value,
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
    let existing_object = decrypt_and_decode(
        storage,
        group,
        resource,
        &existing_kv.key,
        &existing_kv.value,
    )?;

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

    let mut object = existing_object.clone();
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
        &kind,
        group,
        version,
        resource,
        key,
        &existing_kv,
        &existing_object,
        namespace,
        object,
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
) -> Result<UpdateOutcome, Error> {
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

    if dry_run {
        return Ok(UpdateOutcome::Updated(object));
    }

    let api_version = if group.is_empty() {
        version.to_string()
    } else {
        format!("{group}/{version}")
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
    Ok(UpdateOutcome::Updated(object))
}
