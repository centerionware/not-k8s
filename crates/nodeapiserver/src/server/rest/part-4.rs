
/// The "prepare" half of [`server_side_apply`]: resolves the resource,
/// reads the current object (if any), runs the real `updater::apply`
/// orchestration, rebuilds `managedFields`, and validates/defaults the
/// result — everything short of the actual `Txn` write, so a caller can
/// run Group J admission against the real candidate object in between
/// (`server::listener`'s own `PATCH` branch does exactly this for
/// `LimitRanger`, mirroring how [`patch_prepare`]/[`patch_persist`]
/// already split for the same reason).
pub async fn apply_prepare(storage: &mut StorageClient, group: &str, version: &str, resource: &str, namespace: Option<&str>, name: &str, manager: &str, force: bool, config: &Value) -> Result<ApplyPrepareOutcome, Error> {
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

    Ok(ApplyPrepareOutcome::Ready(object, ApplyContext { schema, storage_open_api_schema: resolved.storage_open_api_schema, kind: resolved.kind, conversion_webhook: resolved.conversion_webhook, key, existing: Some((existing_kv, live)) }))
}

fn prune_runtime_schema(schema: Option<&Value>, value: Value) -> Value {
    match schema {
        Some(schema) => apiextensions::schema_pruning::prune(schema, &value),
        None => value,
    }
}

/// The "persist" half of [`server_side_apply`]: writes `object` (the
/// candidate [`apply_prepare`] produced, possibly further mutated by
/// admission in between) with whichever real `Txn` idiom
/// [`ApplyContext::existing`] calls for.
pub async fn apply_persist(storage: &mut StorageClient, group: &str, version: &str, resource: &str, namespace: Option<&str>, context: ApplyContext, mut object: Value, dry_run: bool) -> Result<ApplyOutcome, Error> {
    let Some((existing_kv, live)) = context.existing else {
        object = convert_to_storage_version(storage, group, version, context.conversion_webhook.as_ref(), object).await?;
        object = match revalidate_storage_object(context.storage_open_api_schema.as_ref(), object) {
            Ok(object) => object,
            Err(violations) => return Ok(ApplyOutcome::Invalid(violations)),
        };
        if dry_run {
            let object = convert_to_requested_version(storage, group, version, &context.kind, context.conversion_webhook.as_ref(), object).await?;
            return Ok(ApplyOutcome::Applied(object));
        }
        let stored_version = context.conversion_webhook.as_ref().map_or(version, |conversion| conversion.storage_version.as_str());
        let api_version = if group.is_empty() { stored_version.to_string() } else { format!("{group}/{stored_version}") };
        let object_bytes = match context.schema {
            Some(schema) => protobuf::encode_message(schema, &object)?,
            None => serde_json::to_vec(&object).map_err(protobuf::Error::Json)?,
        };
        let envelope = protobuf::wrap_unknown(&api_version, &context.kind, &object_bytes);
        let compare = pb::Compare {
            key: context.key.clone().into_bytes(),
            result: pb::compare::CompareResult::Equal as i32,
            target: pb::compare::CompareTarget::Mod as i32,
            target_union: Some(pb::compare::TargetUnion::ModRevision(0)),
            range_end: Vec::new(),
        };
        let envelope = encrypt_for_storage(storage, group, resource, context.key.as_bytes(), &envelope)?;
        let put = pb::PutRequest { key: context.key.into_bytes(), value: envelope, ..Default::default() };
        let txn = pb::TxnRequest {
            compare: vec![compare],
            success: vec![pb::RequestOp { request: Some(pb::request_op::Request::RequestPut(put)) }],
            failure: vec![],
        };
        let resp = storage.txn(txn).await?;
        if !resp.succeeded {
            // Lost the race: something else created this key between
            // `apply_prepare`'s own read and this write.
            return Ok(ApplyOutcome::Conflict(Vec::new()));
        }
        let revision = resp.header.map(|h| h.revision).unwrap_or(0);
        set_metadata_field(&mut object, "resourceVersion", Value::String(revision.to_string()));
        let object = convert_to_requested_version(storage, group, version, &context.kind, context.conversion_webhook.as_ref(), object).await?;
        return Ok(ApplyOutcome::Applied(object));
    };

    match persist_update(storage, context.schema, None, context.storage_open_api_schema.as_ref(), &context.kind, group, version, resource, context.key, &existing_kv, &live, namespace, object, dry_run, context.conversion_webhook, None, "", true).await? {
        UpdateOutcome::Updated(v) => Ok(ApplyOutcome::Applied(v)),
        // Lost the optimistic-concurrency race between `apply_prepare`'s
        // own read and this write -- a real, if rare, "retry and see
        // fresh conflicts" situation `updater::apply`'s own conflict
        // detection can't catch by itself, since it never re-reads
        // storage. Reported the same way as an ownership conflict (an
        // empty list, since no *manager* conflict was actually detected)
        // rather than inventing a third outcome variant this early
        // caller-side distinction doesn't otherwise need.
        UpdateOutcome::Conflict => Ok(ApplyOutcome::Conflict(Vec::new())),
        other => unreachable!("persist_update only ever returns Updated or Conflict for an already-decoded, already-validated object: {other:?}"),
    }
}

/// `scheme::name_format`'s validators, wired to the resources this crate
/// has actually verified a real per-type rule for
/// (`apimachinery/pkg/api/validation/generic.go`, confirmed directly):
/// `namespaces` (core group) uses `NameIsDNSLabel`
/// (`ValidateNamespaceName = NameIsDNSLabel`), `serviceaccounts` (core
/// group) uses `NameIsDNSSubdomain` (`ValidateServiceAccountName =
/// NameIsDNSSubdomain`). Every other `(group, resource)` returns no
/// violations at all — not because every other name is assumed valid,
/// but because this crate hasn't verified which real validator applies
/// to it yet; see `scheme::name_format`'s own doc comment for why that
/// mapping isn't a generically-derivable table. Extend this match one
/// verified entry at a time, the same way `scheme::defaulting`'s own
/// concrete case (`ContainerPort.protocol`) was landed and proven before
/// generalizing.
fn name_format_violations(group: &str, resource: &str, name: &str) -> Vec<String> {
    match (group, resource) {
        ("", "namespaces") => crate::scheme::name_format::is_dns1123_label(name),
        ("", "serviceaccounts") => crate::scheme::name_format::is_dns1123_subdomain(name),
        // `pkg/apis/core/validation/validation.go` (release-1.34, fetched
        // and grepped directly), each a literal `var Validate<Kind>Name =
        // apimachineryvalidation.NameIsDNSSubdomain` declaration: Pod,
        // ReplicationController, Node, LimitRange, ResourceQuota, Secret,
        // Endpoints, PersistentVolume, ConfigMap. All ten (including the
        // two already above) resolve to the core (`""`) group — confirmed
        // against the vendored `api__v1_openapi.json` `paths` table, not
        // assumed from this being the "core" validation file (some of its
        // other `var`s, e.g. `ValidatePriorityClassName`/
        // `ValidateResourceClaimName`, are for non-core-group resources
        // and are deliberately NOT wired here until their real group is
        // verified the same way).
        ("", "pods") => crate::scheme::name_format::is_dns1123_subdomain(name),
        ("", "replicationcontrollers") => crate::scheme::name_format::is_dns1123_subdomain(name),
        ("", "nodes") => crate::scheme::name_format::is_dns1123_subdomain(name),
        ("", "limitranges") => crate::scheme::name_format::is_dns1123_subdomain(name),
        ("", "resourcequotas") => crate::scheme::name_format::is_dns1123_subdomain(name),
        ("", "secrets") => crate::scheme::name_format::is_dns1123_subdomain(name),
        ("", "endpoints") => crate::scheme::name_format::is_dns1123_subdomain(name),
        ("", "persistentvolumes") => crate::scheme::name_format::is_dns1123_subdomain(name),
        ("", "configmaps") => crate::scheme::name_format::is_dns1123_subdomain(name),
        // `ValidateServiceCreate` (same file, lines ~6655-6685, read in
        // full): normally `ValidateServiceName = NameIsDNS1035Label`,
        // relaxed to `NameIsDNSLabel` only behind the
        // `RelaxedServiceNameValidation` feature gate (alpha, default
        // off). This crate has no feature-gate system, so the honest
        // default is the gate's default-off behavior: DNS1035Label.
        ("", "services") => crate::scheme::name_format::is_dns1035_label(name),
        // Non-core groups, each confirmed two ways: the real
        // `var Validate<Kind>Name = apimachineryvalidation.NameIsDNSSubdomain`
        // declaration AND the real per-type `Validate<Kind>` function that
        // actually applies it to that type's own `ObjectMeta` (not just a
        // same-named field elsewhere — `ValidateClassName`, for one, is
        // also used to check *referenced* `storageClassName` fields on
        // PV/PVC, which is a different check entirely from this one), plus
        // the group/version cross-checked against the vendored spec's own
        // `paths` table:
        // - `priorityclasses` (scheduling.k8s.io/v1):
        //   `ValidatePriorityClass` -> `NameIsDNSSubdomain` directly
        //   (inlined, not the `ValidatePriorityClassName` var — same rule).
        //   Named honestly: real upstream also forbids a `system-`-prefixed
        //   name unless it's one of a fixed predefined set
        //   (`IsKnownSystemPriorityClass`) — that check is NOT ported here,
        //   only the DNS-subdomain shape.
        // - `resourceclaims`/`resourceclaimtemplates` (resource.k8s.io/v1):
        //   `ValidateResourceClaim`/`ValidateResourceClaimTemplate` ->
        //   `ValidateResourceClaimName`/`ValidateResourceClaimTemplateName`
        //   (`pkg/apis/resource/validation/validation.go`, confirmed).
        // - `storageclasses` (storage.k8s.io/v1): `ValidateStorageClass` ->
        //   `ValidateClassName` (`pkg/apis/storage/validation/validation.go`,
        //   confirmed this is really StorageClass's own object-name check,
        //   not only the referenced-field usage).
        ("scheduling.k8s.io", "priorityclasses") => crate::scheme::name_format::is_dns1123_subdomain(name),
        ("resource.k8s.io", "resourceclaims") => crate::scheme::name_format::is_dns1123_subdomain(name),
        ("resource.k8s.io", "resourceclaimtemplates") => crate::scheme::name_format::is_dns1123_subdomain(name),
        ("storage.k8s.io", "storageclasses") => crate::scheme::name_format::is_dns1123_subdomain(name),
        // More non-core groups, same two-way verification (real
        // per-type `Validate<Kind>[Create]` function confirmed to apply
        // the var to that type's own `ObjectMeta`, real group confirmed
        // against that group's own vendored spec `paths` table):
        // `apps/v1`: ControllerRevision, DaemonSet, Deployment, ReplicaSet
        // (`pkg/apis/apps/validation/validation.go`).
        // `networking.k8s.io/v1`: Ingress, IngressClass, ServiceCIDR
        // (`pkg/apis/networking/validation/validation.go`).
        // `discovery.k8s.io/v1`: EndpointSlice
        // (`pkg/apis/discovery/validation/validation.go`).
        // `flowcontrol.apiserver.k8s.io/v1`: FlowSchema,
        // PriorityLevelConfiguration
        // (`pkg/apis/flowcontrol/validation/validation.go`).
        // `node.k8s.io/v1`: RuntimeClass — inlines `NameIsDNSSubdomain`
        // directly rather than through a named var, same rule
        // (`pkg/apis/node/validation/validation.go`).
        // `coordination.k8s.io/v1`: Lease — same inlined-not-var pattern
        // (`pkg/apis/coordination/validation/validation.go`).
        ("apps", "controllerrevisions") => crate::scheme::name_format::is_dns1123_subdomain(name),
        ("apps", "daemonsets") => crate::scheme::name_format::is_dns1123_subdomain(name),
        ("apps", "deployments") => crate::scheme::name_format::is_dns1123_subdomain(name),
        ("apps", "replicasets") => crate::scheme::name_format::is_dns1123_subdomain(name),
        ("networking.k8s.io", "ingresses") => crate::scheme::name_format::is_dns1123_subdomain(name),
        ("networking.k8s.io", "ingressclasses") => crate::scheme::name_format::is_dns1123_subdomain(name),
        ("networking.k8s.io", "servicecidrs") => crate::scheme::name_format::is_dns1123_subdomain(name),
        ("discovery.k8s.io", "endpointslices") => crate::scheme::name_format::is_dns1123_subdomain(name),
        ("flowcontrol.apiserver.k8s.io", "flowschemas") => crate::scheme::name_format::is_dns1123_subdomain(name),
        ("flowcontrol.apiserver.k8s.io", "prioritylevelconfigurations") => crate::scheme::name_format::is_dns1123_subdomain(name),
        ("node.k8s.io", "runtimeclasses") => crate::scheme::name_format::is_dns1123_subdomain(name),
        ("coordination.k8s.io", "leases") => crate::scheme::name_format::is_dns1123_subdomain(name),
        _ => Vec::new(),
    }
}

/// No-ops (rather than panicking, matching this crate's established
/// "malformed/adversarial input degrades gracefully" posture) if `object`
/// isn't itself a JSON object — `serde_json::Value`'s `IndexMut` panics
/// on a non-object, non-null receiver, and a request body that made it
/// this far without being an object at all is a real, if unlikely, case
/// (an empty-`required`-list schema lets `validate_required`/
/// `validate_types` both pass on one).
fn set_metadata_field(object: &mut Value, field: &str, value: Value) {
    let Some(map) = object.as_object_mut() else { return };
    let metadata = map.entry("metadata").or_insert_with(|| json!({}));
    if !metadata.is_object() {
        *metadata = json!({});
    }
    metadata[field] = value;
}

fn preserve_managed_fields(existing: &Value, object: &mut Value) {
    if let Some(fields) = existing.pointer("/metadata/managedFields").cloned() {
        set_metadata_field(object, "managedFields", fields);
    } else if let Some(metadata) = object.get_mut("metadata").and_then(Value::as_object_mut) {
        metadata.remove("managedFields");
    }
}

fn strip_managed_field_system_fields(mut object: Value) -> Value {
    if let Some(metadata) = object.get_mut("metadata").and_then(Value::as_object_mut) {
        for field in ["resourceVersion", "creationTimestamp", "selfLink", "uid", "managedFields"] {
            metadata.remove(field);
        }
    }
    object
}

fn reconcile_managed_fields(
    schema: Option<&str>,
    open_api_schema: Option<&Value>,
    existing: &Value,
    mut object: Value,
    field_manager: Option<&str>,
    operation: &str,
    subresource: &str,
    group: &str,
    version: &str,
) -> Value {
    let Some(manager) = field_manager.filter(|manager| !manager.is_empty()) else {
        preserve_managed_fields(existing, &mut object);
        return object;
    };

    let Some(manager_schema) = schema else {
        let Some(manager_schema) = open_api_schema else {
            preserve_managed_fields(existing, &mut object);
            return object;
        };
        return reconcile_runtime_managed_fields(manager_schema, existing, object, manager, operation, subresource, group, version);
    };

    let previous = existing.pointer("/metadata/managedFields").cloned().unwrap_or_else(|| Value::Array(Vec::new()));
    let entries = crate::patch::managed_fields::parse_managed_fields(&previous).unwrap_or_default();
    let managers = crate::patch::managed_fields::to_managers_map(&entries);
    let live = strip_managed_field_system_fields(existing.clone());
    let new = strip_managed_field_system_fields(object.clone());
    let manager_key = if subresource.is_empty() { manager.to_string() } else { format!("{manager}/{subresource}") };
    let managers = crate::patch::updater::apply_update(manager_schema, &live, &new, &managers, &manager_key);
    let api_version = if group.is_empty() { version.to_string() } else { format!("{group}/{version}") };
    let rebuilt = crate::patch::managed_fields::rebuild_managed_fields(&entries, &managers, manager, subresource, operation, &api_version, Some(&now_rfc3339()));
    set_metadata_field(&mut object, "managedFields", crate::patch::managed_fields::render_managed_fields(&rebuilt));
    object
}

fn reconcile_runtime_managed_fields(
    schema: &Value,
    existing: &Value,
    mut object: Value,
    manager: &str,
    operation: &str,
    subresource: &str,
    group: &str,
    version: &str,
) -> Value {
    let previous = existing.pointer("/metadata/managedFields").cloned().unwrap_or_else(|| Value::Array(Vec::new()));
    let entries = crate::patch::managed_fields::parse_managed_fields(&previous).unwrap_or_default();
    let managers = crate::patch::managed_fields::to_managers_map(&entries);
    let live = strip_managed_field_system_fields(existing.clone());
    let new = strip_managed_field_system_fields(object.clone());
    let manager_key = if subresource.is_empty() { manager.to_string() } else { format!("{manager}/{subresource}") };
    let managers = crate::apiextensions::schema_apply::apply_update(schema, &live, &new, &managers, &manager_key);
    let api_version = if group.is_empty() { version.to_string() } else { format!("{group}/{version}") };
    let rebuilt = crate::patch::managed_fields::rebuild_managed_fields(&entries, &managers, manager, subresource, operation, &api_version, Some(&now_rfc3339()));
    set_metadata_field(&mut object, "managedFields", crate::patch::managed_fields::render_managed_fields(&rebuilt));
    object
}

fn has_finalizers(object: &Value) -> bool {
    object
        .pointer("/metadata/finalizers")
        .and_then(Value::as_array)
        .is_some_and(|finalizers| !finalizers.is_empty())
}

fn has_deletion_timestamp(object: &Value) -> bool {
    object
        .pointer("/metadata/deletionTimestamp")
        .is_some_and(|timestamp| !timestamp.is_null())
}

/// Allocates the short suffix used by the API server for generateName.
fn generate_name(prefix: &str) -> String {
    let suffix = uuid::Uuid::new_v4().simple().to_string();
    format!("{prefix}{}", &suffix[..5])
}

fn set_type_metadata(object: &mut Value, kind: &str, api_version: &str) {
    let Some(map) = object.as_object_mut() else { return };
    map.insert("kind".to_string(), Value::String(kind.to_string()));
    map.insert("apiVersion".to_string(), Value::String(api_version.to_string()));
}

/// Second-precision RFC3339 with a `Z` suffix (`"2026-08-20T09:30:00Z"`)
/// — matches real upstream's own `metav1.Time` marshaling, which never
/// carries sub-second precision.
fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

#[derive(Debug, PartialEq)]
pub enum DeleteOutcome {
    /// The object as it was immediately before deletion — real upstream's
    /// own synchronous-delete response shape (not a bare `Status`, unless
    /// the caller specifically asked for one, which this build doesn't
    /// yet distinguish).
    Deleted(Value),
    UnknownResource,
    ObjectNotFound,
    /// The requested `resourceVersion` or `uid` did not match the live
    /// object. Kubernetes reports this as a conflict and leaves it intact.
    PreconditionFailed,
}

/// Deletes a single object. `namespace: None` for a cluster-scoped
/// resource, same convention as [`get`]/[`list`]/[`create`].
pub async fn delete(storage: &mut StorageClient, group: &str, version: &str, resource: &str, namespace: Option<&str>, name: &str) -> Result<DeleteOutcome, Error> {
    delete_with_options(storage, group, version, resource, namespace, name, None, false).await
}

/// The subset of Kubernetes `DeleteOptions.preconditions` that can be
/// enforced against nodestore's MVCC-backed objects.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DeletePreconditions {
    pub resource_version: Option<String>,
    pub uid: Option<String>,
}

/// Deletes a single object with optional `DeleteOptions` preconditions and
/// `dryRun=All`. The read and delete/termination marker are joined by an
/// MVCC compare so a concurrent update cannot make a validated delete remove
/// or mark a newer object.
pub async fn delete_with_options(
    storage: &mut StorageClient,
    group: &str,
    version: &str,
    resource: &str,
    namespace: Option<&str>,
    name: &str,
    preconditions: Option<&DeletePreconditions>,
    dry_run: bool,
) -> Result<DeleteOutcome, Error> {
    let Some(resolved) = resolve_resource(storage, group, version, resource).await? else {
        return Ok(DeleteOutcome::UnknownResource);
    };
    let key = keys::object_key(group, resource, namespace, name);
    let current = storage.range(RangeRequest { key: key.clone().into_bytes(), ..Default::default() }).await?;
    let Some(prev) = current.kvs.into_iter().next() else {
        return Ok(DeleteOutcome::ObjectNotFound);
    };
    let mut object = decrypt_and_decode_with_rotation(storage, group, resource, &prev.key, &prev.value, prev.mod_revision).await?;
    set_metadata_field(&mut object, "resourceVersion", Value::String(prev.mod_revision.to_string()));
    if let Some(preconditions) = preconditions {
        if let Some(resource_version) = &preconditions.resource_version {
            let matches = resource_version.parse::<i64>().ok() == Some(prev.mod_revision);
            if !matches {
                return Ok(DeleteOutcome::PreconditionFailed);
            }
        }
        if let Some(uid) = &preconditions.uid {
            if object.pointer("/metadata/uid").and_then(Value::as_str) != Some(uid.as_str()) {
                return Ok(DeleteOutcome::PreconditionFailed);
            }
        }
    }
    let kind = object["kind"].as_str().unwrap_or("Unknown").to_string();

    // A delete request against an object with finalizers is a graceful
    // deletion request, not an immediate storage delete. This is the
    // generic registry behavior that lets controllers observe the
    // deletionTimestamp and remove their own finalizer before the object is
    // physically removed.
    if has_finalizers(&object) {
        if has_deletion_timestamp(&object) {
            let object = convert_to_requested_version(storage, group, version, &kind, resolved.conversion_webhook.as_ref(), object).await?;
            return Ok(DeleteOutcome::Deleted(object));
        }
        set_metadata_field(&mut object, "deletionTimestamp", Value::String(now_rfc3339()));
        if dry_run {
            let object = convert_to_requested_version(storage, group, version, &kind, resolved.conversion_webhook.as_ref(), object).await?;
            return Ok(DeleteOutcome::Deleted(object));
        }
        let object_bytes = match resolved.schema {
            Some(schema) => protobuf::encode_message(schema, &object)?,
            None => serde_json::to_vec(&object).map_err(protobuf::Error::Json)?,
        };
        let stored_version = resolved.conversion_webhook.as_ref().map_or(version, |conversion| conversion.storage_version.as_str());
        let api_version = if group.is_empty() { stored_version.to_string() } else { format!("{group}/{stored_version}") };
        let envelope = encrypt_for_storage(storage, group, resource, key.as_bytes(), &protobuf::wrap_unknown(&api_version, &kind, &object_bytes))?;
        let compare = pb::Compare {
            key: key.clone().into_bytes(),
            result: pb::compare::CompareResult::Equal as i32,
            target: pb::compare::CompareTarget::Mod as i32,
            target_union: Some(pb::compare::TargetUnion::ModRevision(prev.mod_revision)),
            range_end: Vec::new(),
        };
        let txn = pb::TxnRequest {
            compare: vec![compare],
            success: vec![pb::RequestOp {
                request: Some(pb::request_op::Request::RequestPut(pb::PutRequest {
                    key: key.clone().into_bytes(),
                    value: envelope,
                    ..Default::default()
                })),
            }],
            failure: vec![],
        };
        let response = storage.txn(txn).await?;
        if !response.succeeded {
            return Ok(DeleteOutcome::PreconditionFailed);
        }
        let revision = response.header.map(|header| header.revision).unwrap_or(0);
        set_metadata_field(&mut object, "resourceVersion", Value::String(revision.to_string()));
        let object = convert_to_requested_version(storage, group, version, &kind, resolved.conversion_webhook.as_ref(), object).await?;
        return Ok(DeleteOutcome::Deleted(object));
    }

    if dry_run {
        let object = convert_to_requested_version(storage, group, version, &kind, resolved.conversion_webhook.as_ref(), object).await?;
        return Ok(DeleteOutcome::Deleted(object));
    }

    let compare = pb::Compare {
        key: key.clone().into_bytes(),
        result: pb::compare::CompareResult::Equal as i32,
        target: pb::compare::CompareTarget::Mod as i32,
        target_union: Some(pb::compare::TargetUnion::ModRevision(prev.mod_revision)),
        range_end: Vec::new(),
    };
    let txn = pb::TxnRequest {
        compare: vec![compare],
        success: vec![pb::RequestOp {
            request: Some(pb::request_op::Request::RequestDeleteRange(pb::DeleteRangeRequest {
                key: key.into_bytes(),
                prev_kv: true,
                ..Default::default()
            })),
        }],
        failure: vec![],
    };
    let response = storage.txn(txn).await?;
    if !response.succeeded {
        return Ok(DeleteOutcome::PreconditionFailed);
    }
    let object = convert_to_requested_version(storage, group, version, &kind, resolved.conversion_webhook.as_ref(), object).await?;
    Ok(DeleteOutcome::Deleted(object))
}

#[derive(Debug, PartialEq)]
pub enum DeleteCollectionOutcome {
    /// The `<Kind>List` of every object that matched, exactly as it
    /// listed immediately before any of them were deleted — real
    /// upstream's own `Store.DeleteCollection` response shape (it
    /// returns the `List` object it read at the start, not one rebuilt
    /// after the fact).
    Deleted(Value),
    UnknownResource,
}

/// Lists the objects selected by a collection delete without changing them.
/// The listener uses this first so it can run admission against each matched
/// object before calling [`delete`].
pub async fn list_delete_collection(
    storage: &mut StorageClient,
    group: &str,
    version: &str,
    resource: &str,
    namespace: Option<&str>,
    label_selector: &str,
    field_selector: &str,
) -> Result<DeleteCollectionOutcome, Error> {
    let listed = list(
        storage,
        None,
        group,
        version,
        resource,
        namespace,
        label_selector,
        field_selector,
        0,
        "",
    )
    .await?;
    let ListOutcome::Found(list_value) = listed else {
        return Ok(DeleteCollectionOutcome::UnknownResource);
    };
    Ok(DeleteCollectionOutcome::Deleted(list_value))
}

/// Real upstream's own `Store.DeleteCollection`
/// (`k8s.io/apiserver/pkg/registry/generic/registry/store.go`, fetched
/// and read directly), scoped down: lists every object matching
/// `label_selector`/`field_selector` (reusing [`list`]'s own selector
/// parsing — the exact same filtering a real `DELETE .../pods` collection
/// request would apply), then deletes each one by name via [`delete`],
/// silently ignoring one that's already gone (`ObjectNotFound` — matches
/// real upstream's own `!apierrors.IsNotFound(err)` guard: a concurrent
/// delete of the same object isn't a collection-delete failure). Returns
/// the pre-deletion `List`, the same real response shape a single
/// `DELETE`'s own "the object as it was immediately before deletion"
/// convention already established for one object at a time.
/// **Named, honest simplification**: real upstream deletes with a
/// worker pool (`DeleteCollectionWorkers`, concurrent); this port
/// deletes sequentially. It also always lists everything in one
/// unpaginated shot (`limit: 0`) regardless of how large the collection
/// is — real upstream's own collection delete paginates its internal
/// listing too, which this doesn't. A per-item deletion error *other
/// than* not-found still aborts the whole call and surfaces as a real
/// `500` — real upstream's own posture too (`errs <- err` stops the
/// collection short).
pub async fn delete_collection(storage: &mut StorageClient, group: &str, version: &str, resource: &str, namespace: Option<&str>, label_selector: &str, field_selector: &str) -> Result<DeleteCollectionOutcome, Error> {
    let listed = list_delete_collection(storage, group, version, resource, namespace, label_selector, field_selector).await?;
    let DeleteCollectionOutcome::Deleted(list_value) = listed else {
        return Ok(DeleteCollectionOutcome::UnknownResource);
    };
    let items = list_value["items"].as_array().cloned().unwrap_or_default();
    for item in &items {
        let Some(name) = item.pointer("/metadata/name").and_then(Value::as_str) else { continue };
        match delete(storage, group, version, resource, namespace, name).await? {
            DeleteOutcome::ObjectNotFound => {}
            DeleteOutcome::Deleted(_) | DeleteOutcome::UnknownResource | DeleteOutcome::PreconditionFailed => {}
        }
    }
    Ok(DeleteCollectionOutcome::Deleted(list_value))
}
