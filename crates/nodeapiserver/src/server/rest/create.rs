#[derive(Debug, PartialEq)]
pub enum CreateOutcome {
    /// The stored object, exactly as written (defaults applied,
    /// `creationTimestamp`/`uid`/`resourceVersion` set for real).
    Created(Value),
    UnknownResource,
    /// Neither `metadata.name` nor a usable `metadata.generateName` was
    /// present in the submitted body.
    MissingName,
    /// `metadata.namespace` in the body disagreed with the URL's own
    /// namespace — real upstream rejects this rather than silently
    /// preferring one over the other.
    NamespaceMismatch,
    /// An object already exists at this key — real upstream's own
    /// `AlreadyExists` outcome.
    AlreadyExists,
    /// `scheme::validation`'s own findings, formatted as one message per
    /// violation (`"containers[1].name: Required value"`-shaped) — the
    /// caller's job to turn into a real `422 Unprocessable Entity`.
    Invalid(Vec<String>),
    /// No usable compiled or runtime structural schema was available for
    /// the resolved resource. Established CRDs normally carry the latter;
    /// this remains a defensive outcome for malformed or legacy CRD data.
    UnsupportedForCrd,
}

/// Creates a new object. `namespace: None` for a cluster-scoped resource,
/// same convention as [`get`]/[`list`]. `body` is the client's raw
/// submitted object, decoded but otherwise untouched — this function
/// validates and defaults it, it doesn't trust it.
pub async fn create(
    storage: &mut StorageClient,
    group: &str,
    version: &str,
    resource: &str,
    namespace: Option<&str>,
    body: &Value,
) -> Result<CreateOutcome, Error> {
    create_with_options_and_manager(
        storage, group, version, resource, namespace, body, false, None,
    )
    .await
}

/// [`create`] with the real Kubernetes `dryRun=All` write option. Dry-run
/// still resolves, validates, defaults, and checks for an existing object,
/// but never changes nodestore.
pub async fn create_with_options(
    storage: &mut StorageClient,
    group: &str,
    version: &str,
    resource: &str,
    namespace: Option<&str>,
    body: &Value,
    dry_run: bool,
) -> Result<CreateOutcome, Error> {
    create_with_options_and_manager(
        storage, group, version, resource, namespace, body, dry_run, None,
    )
    .await
}

/// [`create_with_options`] with the request's field manager. The listener
/// supplies the explicit `fieldManager` or the request's user agent, just as
/// upstream's `managerOrUserAgent` does. Direct REST callers may omit it;
/// their submitted `managedFields` are never trusted or persisted.
pub async fn create_with_options_and_manager(
    storage: &mut StorageClient,
    group: &str,
    version: &str,
    resource: &str,
    namespace: Option<&str>,
    body: &Value,
    dry_run: bool,
    field_manager: Option<&str>,
) -> Result<CreateOutcome, Error> {
    let Some(resolved) = resolve_resource(storage, group, version, resource).await? else {
        return Ok(CreateOutcome::UnknownResource);
    };
    let kind = resolved.kind.as_str();

    let explicit_name = body
        .pointer("/metadata/name")
        .and_then(Value::as_str)
        .filter(|n| !n.is_empty());
    let generated_prefix = body
        .pointer("/metadata/generateName")
        .and_then(Value::as_str)
        .filter(|prefix| !prefix.is_empty());
    let Some(name) = explicit_name
        .map(str::to_string)
        .or_else(|| generated_prefix.map(generate_name))
    else {
        return Ok(CreateOutcome::MissingName);
    };
    let mut submitted_body = body.clone();
    if explicit_name.is_none() {
        set_metadata_field(&mut submitted_body, "name", Value::String(name.clone()));
    }
    let body = &submitted_body;

    if let (Some(ns), Some(body_ns)) = (
        namespace,
        body.pointer("/metadata/namespace").and_then(Value::as_str),
    ) {
        if !body_ns.is_empty() && body_ns != ns {
            return Ok(CreateOutcome::NamespaceMismatch);
        }
    }

    // Group K: structural-schema pruning runs before validation/defaulting,
    // matching real upstream's own order — a field the schema doesn't
    // declare is silently dropped here rather than surfacing as a
    // validation error, the same way real upstream's own CRD handler
    // behaves (`apiextensions::schema_pruning`'s own doc comment).
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
                group, version, kind, body,
            ));
            v
        }
        // Group K: real required/type validation against a CRD's own
        // openAPIV3Schema, when it has one.
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
        name_format_violations(group, resource, &name)
            .into_iter()
            .map(|e| format!("metadata.name: {e}")),
    );
    violations.extend(metadata_format_violations(body));
    // Group K / CEL Phase 3: a CustomResourceDefinition's own
    // `x-kubernetes-validations` rules get their real static cost
    // checked at CRD-acceptance time, real upstream's own posture
    // (`apiextensions::cel_validations`'s own doc comment covers the
    // exact real scope and its one named gap — no `MaxCardinality`
    // multiplication yet).
    if group == "apiextensions.k8s.io" && resource == "customresourcedefinitions" {
        violations.extend(apiextensions::cel_validations::validate_crd_cel_costs(body));
        violations.extend(apiextensions::cel_validations::validate_crd_cel_types(body));
    }
    if !violations.is_empty() {
        return Ok(CreateOutcome::Invalid(violations));
    }

    let mut object = match (resolved.schema, &resolved.open_api_schema) {
        (Some(schema), _) => defaulting::apply_defaults(schema, body),
        (None, Some(open_api_schema)) => {
            apiextensions::schema_defaults::apply_defaults(open_api_schema, body)
        }
        (None, None) => body.clone(),
    };
    object = defaulting::apply_builtin_defaults(group, version, kind, object);
    object = crate::scheme::conversion::to_version(group, version, kind, object);

    // CEL Phase 4: real x-kubernetes-validations rule evaluation against
    // this actual custom resource instance — runs against the
    // fully-defaulted object (real upstream's own ordering: a rule
    // commonly assumes a field already carries its real default, not an
    // absence), `old_value: None` on `CREATE` (real upstream's own
    // `oldSelf` is simply unavailable then, matching
    // `apiextensions::cel_evaluate`'s own doc comment).
    if let Some(open_api_schema) = &resolved.open_api_schema {
        let rule_violations =
            apiextensions::cel_evaluate::validate_object(open_api_schema, &object, None);
        if !rule_violations.is_empty() {
            return Ok(CreateOutcome::Invalid(
                rule_violations.into_iter().map(|v| v.to_string()).collect(),
            ));
        }
    }

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
    if let Some(ns) = namespace {
        set_metadata_field(&mut object, "namespace", Value::String(ns.to_string()));
    }

    // Group K: a CustomResourceDefinition's own `status` is entirely
    // server-computed (`apiextensions::conditions`'s own doc comment
    // covers why this build computes it synchronously right here rather
    // than through a separate async establishing controller) — never
    // trusted from whatever the client's submitted body carried under
    // `status`, same "generic status subresource" posture `update_status`
    // already establishes for every other resource's own status.
    if group == "apiextensions.k8s.io" && resource == "customresourcedefinitions" {
        let other_crds = list_stored_crds(storage).await?;
        object["status"] = apiextensions::conditions::compute_status(
            &object,
            other_crds.iter(),
            &[],
            &now_rfc3339(),
        );
    }

    object = reconcile_managed_fields(
        resolved.schema,
        resolved.open_api_schema.as_ref(),
        &json!({}),
        object,
        field_manager,
        "Update",
        "",
        group,
        version,
        resolved.has_status_subresource,
    );

    // Conversion sees the complete object, including the system metadata
    // generated above. This is the same object shape a webhook receives for
    // an object that is about to be persisted, not the pre-create body.
    object = convert_to_storage_version(
        storage,
        group,
        version,
        resolved.conversion_webhook.as_ref(),
        object,
    )
    .await?;
    object = match revalidate_storage_object(resolved.storage_open_api_schema.as_ref(), object) {
        Ok(object) => object,
        Err(violations) => return Ok(CreateOutcome::Invalid(violations)),
    };

    let key = keys::object_key(group, resource, namespace, &name);
    if dry_run {
        let existing = storage
            .range(RangeRequest {
                key: key.clone().into_bytes(),
                ..Default::default()
            })
            .await?;
        if !existing.kvs.is_empty() {
            return Ok(CreateOutcome::AlreadyExists);
        }
        let object = convert_to_requested_version(
            storage,
            group,
            version,
            kind,
            resolved.conversion_webhook.as_ref(),
            object,
        )
        .await?;
        return Ok(CreateOutcome::Created(object));
    }
    let stored_version = resolved
        .conversion_webhook
        .as_ref()
        .map_or(version, |conversion| conversion.storage_version.as_str());
    let api_version = if group.is_empty() {
        stored_version.to_string()
    } else {
        format!("{group}/{stored_version}")
    };
    let object_bytes = match resolved.schema {
        Some(schema) => protobuf::encode_message(schema, &object)?,
        None => serde_json::to_vec(&object).map_err(protobuf::Error::Json)?,
    };
    let envelope = protobuf::wrap_unknown(&api_version, kind, &object_bytes);

    // Real upstream's own create-only-if-absent idiom, confirmed against
    // nodestore's own server-side comment naming it exactly this
    // (`crates/nodestore/src/store.rs`): a key with no prior write has
    // ModRevision 0, so a Txn that only Puts when ModRevision == 0 can
    // never silently overwrite an existing object.
    let compare = pb::Compare {
        key: key.clone().into_bytes(),
        result: pb::compare::CompareResult::Equal as i32,
        target: pb::compare::CompareTarget::Mod as i32,
        target_union: Some(pb::compare::TargetUnion::ModRevision(0)),
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
        return Ok(CreateOutcome::AlreadyExists);
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
        resolved.conversion_webhook.as_ref(),
        object,
    )
    .await?;
    Ok(CreateOutcome::Created(object))
}
