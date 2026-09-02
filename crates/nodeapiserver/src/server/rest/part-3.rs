
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
pub async fn update_status(storage: &mut StorageClient, group: &str, version: &str, resource: &str, namespace: Option<&str>, name: &str, body: &Value, dry_run: bool) -> Result<UpdateOutcome, Error> {
    update_status_with_manager(storage, group, version, resource, namespace, name, body, dry_run, None).await
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
    let existing_resp = storage.range(RangeRequest { key: key.clone().into_bytes(), ..Default::default() }).await?;
    let Some(existing_kv) = existing_resp.kvs.into_iter().next() else {
        return Ok(UpdateOutcome::ObjectNotFound);
    };
    let existing_object = decrypt_and_decode_with_rotation(storage, group, resource, &existing_kv.key, &existing_kv.value, existing_kv.mod_revision).await?;
    let existing_object_for_request = convert_to_requested_version(storage, group, version, &kind, resolved.conversion_webhook.as_ref(), existing_object.clone()).await?;

    let Some(submitted_rv) = body.pointer("/metadata/resourceVersion").and_then(Value::as_str).and_then(|s| s.parse::<i64>().ok()) else {
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

    persist_update(storage, resolved.schema, resolved.open_api_schema.as_ref(), resolved.storage_open_api_schema.as_ref(), &kind, group, version, resource, key, &existing_kv, &existing_object, namespace, object, dry_run, resolved.conversion_webhook.clone(), field_manager, "status", false).await
}

/// Applies a CRD version's `properties.status` schema to a status-subresource
/// candidate. Wrapping the status value in a synthetic top-level object lets
/// the existing schema walkers report the correct `status.foo` paths and
/// check the status root's own type without duplicating their logic. The
/// returned value is already pruned; it is only written when validation
/// succeeds.
fn validate_crd_status(open_api_schema: &Option<Value>, object: &mut Value) -> Vec<String> {
    let Some(schema) = open_api_schema.as_ref() else { return Vec::new() };
    let Some(status_schema) = schema.pointer("/properties/status") else { return Vec::new() };
    let Some(status) = object.get("status").cloned() else { return Vec::new() };

    let wrapper_schema = json!({
        "type": "object",
        "properties": {"status": status_schema}
    });
    let candidate = json!({"status": status});
    let pruned = apiextensions::schema_pruning::prune(&wrapper_schema, &candidate);
    let mut violations: Vec<String> = apiextensions::schema_validation::validate_required(&wrapper_schema, &pruned)
        .into_iter()
        .map(|violation| format!("{}: Required value", violation.path))
        .collect();
    violations.extend(
        apiextensions::schema_validation::validate_types(&wrapper_schema, &pruned)
            .into_iter()
            .map(|violation| format!("{}: expected type {}, got {}", violation.path, violation.expected, violation.actual_kind)),
    );
    violations.extend(apiextensions::schema_validation::validate_constraints(&wrapper_schema, &pruned));
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
    managed_fields_reconciled: bool,
) -> Result<UpdateOutcome, Error> {
    for field in ["creationTimestamp", "uid"] {
        if let Some(existing_value) = existing_object.pointer(&format!("/metadata/{field}")).cloned() {
            set_metadata_field(&mut object, field, existing_value);
        }
    }
    if let Some(ns) = namespace {
        set_metadata_field(&mut object, "namespace", Value::String(ns.to_string()));
    }

    object = crate::scheme::conversion::to_version(group, version, kind, object);
    if field_manager.is_some() {
        let existing_for_fields = convert_to_requested_version(storage, group, version, kind, conversion_webhook.as_ref(), existing_object.clone()).await?;
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
        );
    } else if !managed_fields_reconciled {
        preserve_managed_fields(existing_object, &mut object);
    }
    object = convert_to_storage_version(storage, group, version, conversion_webhook.as_ref(), object).await?;
    object = match revalidate_storage_object(storage_open_api_schema, object) {
        Ok(object) => object,
        Err(violations) => return Ok(UpdateOutcome::Invalid(violations)),
    };

    // Removing the last finalizer from an object already marked for deletion
    // completes the deletion. This mirrors the generic registry's
    // ShouldDeleteDuringUpdate path: the update is accepted, but the object
    // is removed atomically instead of being written back as a live object.
    if has_deletion_timestamp(existing_object) && !has_finalizers(&object) {
        if dry_run {
            let object = convert_to_requested_version(storage, group, version, kind, conversion_webhook.as_ref(), object).await?;
            return Ok(UpdateOutcome::Updated(object));
        }
        let compare = pb::Compare {
            key: key.clone().into_bytes(),
            result: pb::compare::CompareResult::Equal as i32,
            target: pb::compare::CompareTarget::Mod as i32,
            target_union: Some(pb::compare::TargetUnion::ModRevision(existing_kv.mod_revision)),
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
            return Ok(UpdateOutcome::Conflict);
        }
        let revision = response.header.map(|header| header.revision).unwrap_or(0);
        set_metadata_field(&mut object, "resourceVersion", Value::String(revision.to_string()));
        let object = convert_to_requested_version(storage, group, version, kind, conversion_webhook.as_ref(), object).await?;
        return Ok(UpdateOutcome::Updated(object));
    }

    if dry_run {
        let object = convert_to_requested_version(storage, group, version, kind, conversion_webhook.as_ref(), object).await?;
        return Ok(UpdateOutcome::Updated(object));
    }

    let stored_version = conversion_webhook.as_ref().map_or(version, |conversion| conversion.storage_version.as_str());
    let api_version = if group.is_empty() { stored_version.to_string() } else { format!("{group}/{stored_version}") };
    let object_bytes = match schema {
        Some(schema) => protobuf::encode_message(schema, &object)?,
        None => serde_json::to_vec(&object).map_err(protobuf::Error::Json)?,
    };
    let envelope = protobuf::wrap_unknown(&api_version, kind, &object_bytes);

    let compare = pb::Compare {
        key: key.clone().into_bytes(),
        result: pb::compare::CompareResult::Equal as i32,
        target: pb::compare::CompareTarget::Mod as i32,
        target_union: Some(pb::compare::TargetUnion::ModRevision(existing_kv.mod_revision)),
        range_end: Vec::new(),
    };
    let envelope = encrypt_for_storage(storage, group, resource, key.as_bytes(), &envelope)?;
    let put = pb::PutRequest { key: key.into_bytes(), value: envelope, ..Default::default() };
    let txn = pb::TxnRequest {
        compare: vec![compare],
        success: vec![pb::RequestOp { request: Some(pb::request_op::Request::RequestPut(put)) }],
        failure: vec![],
    };
    let resp = storage.txn(txn).await?;
    if !resp.succeeded {
        // Lost the race: something else wrote to this key between our
        // read above and this write.
        return Ok(UpdateOutcome::Conflict);
    }

    let revision = resp.header.map(|h| h.revision).unwrap_or(0);
    set_metadata_field(&mut object, "resourceVersion", Value::String(revision.to_string()));
    let object = convert_to_requested_version(storage, group, version, kind, conversion_webhook.as_ref(), object).await?;
    Ok(UpdateOutcome::Updated(object))
}

/// The three real patch media types this build understands, and which
/// `patch::*` module applies each. The request handler separately applies
/// Kubernetes' default strategy when a request has no `Content-Type`:
/// strategic merge for built-in resources and merge patch for CRDs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PatchKind {
    Json,
    Merge,
    StrategicMerge,
}

/// Real upstream's own three patch `Content-Type` media types
/// (`k8s.io/apimachinery/pkg/types`): `application/json-patch+json`
/// (RFC 6902), `application/merge-patch+json` (RFC 7386),
/// `application/strategic-merge-patch+json` (k8s-specific). Server-Side
/// Apply's own `application/apply-patch+yaml` is routed separately by the
/// listener because it has different query parameters and bookkeeping than
/// the three ordinary patch kinds below.
pub fn patch_kind_for_content_type(content_type: &str) -> Option<PatchKind> {
    match content_type.split(';').next().unwrap_or("").trim() {
        "application/json-patch+json" => Some(PatchKind::Json),
        "application/merge-patch+json" => Some(PatchKind::Merge),
        "application/strategic-merge-patch+json" => Some(PatchKind::StrategicMerge),
        _ => None,
    }
}

/// Kubernetes' default patch strategy when a request omits `Content-Type`.
/// Built-in resources have compiled schemas and therefore use strategic
/// merge; CRD-defined resources use JSON merge patch because they do not
/// have the generated strategic-merge metadata used by built-ins.
pub fn default_patch_kind(is_crd: bool) -> PatchKind {
    if is_crd { PatchKind::Merge } else { PatchKind::StrategicMerge }
}

/// Resolves the resource and returns the default patch strategy for a
/// request with no `Content-Type`. `None` means the URL names no resource
/// this server knows about, so the listener can preserve its normal 404
/// response rather than reporting a media-type error.
pub async fn default_patch_kind_for_request(storage: &mut StorageClient, group: &str, version: &str, resource: &str) -> Result<Option<PatchKind>, Error> {
    Ok(resolve_resource(storage, group, version, resource).await?.map(|resolved| default_patch_kind(resolved.schema.is_none())))
}

/// Apply a CEL `MutatingAdmissionPolicy` apply configuration to an admission
/// object. Apply configurations use the same strategic-merge rules as the
/// server's strategic-merge PATCH path; built-ins use their generated schema
/// and CRDs use their runtime OpenAPI schema. A resource without either
/// schema falls back to JSON merge semantics, which preserves the generic
/// server's behavior for schema-less resources.
pub async fn apply_admission_configuration(storage: &mut StorageClient, group: &str, version: &str, resource: &str, existing: &Value, configuration: &Value) -> Result<Value, Error> {
    let Some(resolved) = resolve_resource(storage, group, version, resource).await? else {
        return Err(Error::UnknownResource);
    };
    Ok(match (resolved.schema, resolved.open_api_schema.as_ref()) {
        (Some(schema), _) => crate::patch::strategic_merge::apply(schema, existing, configuration),
        (None, Some(schema)) => apiextensions::schema_strategic_merge::apply(schema, existing, configuration),
        (None, None) => {
            let mut object = existing.clone();
            crate::patch::merge_patch::apply(&mut object, configuration);
            object
        }
    })
}

/// The context [`patch_prepare`] hands back to [`patch_persist`] once a
/// patch has been applied but before it's validated/persisted — enough
/// to run Group J admission against the real candidate object in
/// between (`server::listener`'s own `PATCH` branch does exactly this
/// for `LimitRanger`), without re-fetching or re-applying the patch a
/// second time.
#[derive(Debug)]
pub struct PatchContext {
    /// `None` for a CRD-defined resource — see [`apply_patch`]'s own doc
    /// comment for what that rules out (`strategic-merge-patch`) and
    /// what it doesn't (`JSON Patch`/`Merge Patch`, and
    /// [`patch_persist`]'s own schema-driven defaulting, which falls
    /// back to `open_api_schema` in exactly this case).
    schema: Option<&'static str>,
    open_api_schema: Option<Value>,
    storage_open_api_schema: Option<Value>,
    kind: String,
    conversion_webhook: Option<apiextensions::registry::ConversionWebhook>,
    key: String,
    existing_kv: mvccpb::KeyValue,
    existing_object: Value,
}

#[derive(Debug)]
pub enum PatchPrepareOutcome {
    /// The patch applied cleanly; `candidate` is the resulting object,
    /// not yet validated/defaulted/persisted.
    Ready(Value, PatchContext),
    UnknownResource,
    ObjectNotFound,
    /// The patch itself couldn't be applied (a JSON Patch `test` op
    /// failure, or a malformed patch document).
    Invalid(Vec<String>),
}

/// Applies one of this build's three real patch kinds
/// ([`crate::patch::json_patch`]/[`crate::patch::merge_patch`]/
/// [`crate::patch::strategic_merge`], all landed in Group G) to
/// `existing`. Shared by [`patch_prepare`] (patches the whole object)
/// and [`patch_status`] (patches the whole object too — real upstream's
/// own subresource PATCH semantics: the patch document can reference
/// any path, only the final write is restricted to `.status` — the
/// restriction happens at persist time, not by scoping what the patch
/// itself can touch).
///
/// `schema` is `None` for a CRD-defined resource — `JSON Patch`/`Merge
/// Patch` need no schema at all and work identically either way;
/// `strategic-merge-patch` uses `open_api_schema` instead in that case
/// (`apiextensions::schema_strategic_merge`, the runtime-schema sibling
/// of `crate::patch::strategic_merge`'s own compiled-`ref_schema` walk).
/// `open_api_schema` is `None` too only for a CRD version whose own
/// document carries no schema at all (a real, if unusual, case this
/// build's own read path already tolerates elsewhere — a malformed/
/// legacy document, `apiextensions::registry::CrdResource`'s own doc
/// comment) — a `strategic-merge-patch` against one has no schema of any
/// kind to interpret, a real `Invalid`, not a panic.
fn apply_patch(kind_of_patch: PatchKind, schema: Option<&str>, open_api_schema: Option<&Value>, existing: &Value, patch_doc: &Value) -> Result<Value, String> {
    match kind_of_patch {
        PatchKind::Json => {
            let mut object = existing.clone();
            if crate::patch::json_patch::apply(&mut object, patch_doc).is_err() {
                return Err("the submitted JSON Patch could not be applied".to_string());
            }
            Ok(object)
        }
        PatchKind::Merge => {
            let mut object = existing.clone();
            crate::patch::merge_patch::apply(&mut object, patch_doc);
            Ok(object)
        }
        PatchKind::StrategicMerge => match (schema, open_api_schema) {
            (Some(schema), _) => Ok(crate::patch::strategic_merge::apply(schema, existing, patch_doc)),
            (None, Some(open_api_schema)) => Ok(apiextensions::schema_strategic_merge::apply(open_api_schema, existing, patch_doc)),
            (None, None) => Err("strategic-merge-patch: this resource has no known schema to interpret x-kubernetes-list-type/-list-map-keys against".to_string()),
        },
    }
}

/// Reads the current object and applies one of this build's three real
/// patch kinds to it — the "prepare" half of [`patch`], split out so a
/// caller (`server::listener`) can run Group J admission against the
/// real candidate object before committing to [`patch_persist`].
pub async fn patch_prepare(storage: &mut StorageClient, group: &str, version: &str, resource: &str, namespace: Option<&str>, name: &str, kind_of_patch: PatchKind, patch_doc: &Value) -> Result<PatchPrepareOutcome, Error> {
    let Some(resolved) = resolve_resource(storage, group, version, resource).await? else {
        return Ok(PatchPrepareOutcome::UnknownResource);
    };

    let key = keys::object_key(group, resource, namespace, name);
    let existing_resp = storage.range(RangeRequest { key: key.clone().into_bytes(), ..Default::default() }).await?;
    let Some(existing_kv) = existing_resp.kvs.into_iter().next() else {
        return Ok(PatchPrepareOutcome::ObjectNotFound);
    };
    let existing_object = decrypt_and_decode_with_rotation(storage, group, resource, &existing_kv.key, &existing_kv.value, existing_kv.mod_revision).await?;
    let existing_object_for_request = convert_to_requested_version(storage, group, version, &resolved.kind, resolved.conversion_webhook.as_ref(), existing_object.clone()).await?;

    let patched = match apply_patch(kind_of_patch, resolved.schema, resolved.open_api_schema.as_ref(), &existing_object_for_request, patch_doc) {
        Ok(object) => object,
        Err(msg) => return Ok(PatchPrepareOutcome::Invalid(vec![msg])),
    };

    Ok(PatchPrepareOutcome::Ready(
        patched,
        PatchContext { schema: resolved.schema, open_api_schema: resolved.open_api_schema, storage_open_api_schema: resolved.storage_open_api_schema, kind: resolved.kind, conversion_webhook: resolved.conversion_webhook, key, existing_kv, existing_object },
    ))
}

/// The "persist" half of [`patch`]: validates/defaults `candidate` (the
/// object [`patch_prepare`] produced, possibly further mutated by
/// admission in between) and writes it with the same real optimistic
/// concurrency [`update`] uses (`Txn`-compared-against-`ModRevision`,
/// via the shared [`persist_update`] tail) — no client-submitted
/// `resourceVersion` needed, since the object being patched *is* the one
/// [`patch_prepare`] already read. With `dry_run`, it performs all of the
/// same validation/defaulting and returns the candidate without writing.
pub async fn patch_persist(storage: &mut StorageClient, group: &str, version: &str, resource: &str, namespace: Option<&str>, name: &str, context: PatchContext, candidate: Value, dry_run: bool) -> Result<UpdateOutcome, Error> {
    patch_persist_with_manager(storage, group, version, resource, namespace, name, context, candidate, dry_run, None).await
}

/// [`patch_persist`] with the request's field manager. Ordinary patch writes
/// use the same managed-fields `Update` operation as ordinary PUT writes.
pub async fn patch_persist_with_manager(
    storage: &mut StorageClient,
    group: &str,
    version: &str,
    resource: &str,
    namespace: Option<&str>,
    name: &str,
    context: PatchContext,
    candidate: Value,
    dry_run: bool,
    field_manager: Option<&str>,
) -> Result<UpdateOutcome, Error> {
    // Group K: same pruning `create`/`update` run, same order (before
    // validation/defaulting) — `candidate` is already owned, so this
    // just reassigns it rather than needing the borrow-juggling
    // `create`/`update` need for their own `&Value` parameter.
    let candidate = match &context.open_api_schema {
        Some(open_api_schema) => apiextensions::schema_pruning::prune(open_api_schema, &candidate),
        None => candidate,
    };

    let mut violations: Vec<String> = match (context.schema, &context.open_api_schema) {
        (Some(schema), _) => {
            let mut v: Vec<String> = validation::validate_required(schema, &candidate).into_iter().map(|m| format!("{}: Required value", m.path)).collect();
            v.extend(validation::validate_types(schema, &candidate).into_iter().map(|t| format!("{}: expected type {}, got {}", t.path, t.expected, t.actual_kind)));
            v
        }
        // Group K: same scope `create`'s own CRD branch runs.
        (None, Some(open_api_schema)) => {
            let mut v: Vec<String> = apiextensions::schema_validation::validate_required(open_api_schema, &candidate).into_iter().map(|m| format!("{}: Required value", m.path)).collect();
            v.extend(apiextensions::schema_validation::validate_types(open_api_schema, &candidate).into_iter().map(|t| format!("{}: expected type {}, got {}", t.path, t.expected, t.actual_kind)));
            v.extend(apiextensions::schema_validation::validate_constraints(open_api_schema, &candidate));
            v
        }
        (None, None) => Vec::new(),
    };
    violations.extend(name_format_violations(group, resource, name).into_iter().map(|e| format!("metadata.name: {e}")));
    if !violations.is_empty() {
        return Ok(UpdateOutcome::Invalid(violations));
    }

    let object = match (context.schema, &context.open_api_schema) {
        (Some(schema), _) => defaulting::apply_defaults(schema, &candidate),
        (None, Some(open_api_schema)) => apiextensions::schema_defaults::apply_defaults(open_api_schema, &candidate),
        (None, None) => candidate,
    };

    // CEL Phase 4: same real rule evaluation `create`/`update` both run.
    if let Some(open_api_schema) = &context.open_api_schema {
        let rule_violations = apiextensions::cel_evaluate::validate_object(open_api_schema, &object, Some(&context.existing_object));
        if !rule_violations.is_empty() {
            return Ok(UpdateOutcome::Invalid(rule_violations.into_iter().map(|v| v.to_string()).collect()));
        }
    }

    persist_update(storage, context.schema, context.open_api_schema.as_ref(), context.storage_open_api_schema.as_ref(), &context.kind, group, version, resource, context.key, &context.existing_kv, &context.existing_object, namespace, object, dry_run, context.conversion_webhook, field_manager, "", false).await
}

/// `PATCH .../status` — the patch counterpart to [`update_status`],
/// closing the "PUT-only" gap `docs/APISERVER.md` named for it. Applies
/// the patch to the whole existing object (same
/// [`apply_patch`] `patch_prepare` uses — real upstream's own subresource
/// PATCH semantics let the patch document reference any path), then
/// takes only the result's own `.status` field and merges it onto the
/// existing object exactly the way `update_status` does, so a
/// `strategic-merge-patch+json` `{"status": {...}}` document behaves the
/// same whether it arrives via `PUT` (full replace) or `PATCH` (merged).
/// No client-submitted `resourceVersion` needed, same as `patch_persist`.
/// The CRD status schema is applied to the patched status with the same
/// pruning and local validation as [`update_status`]. Built-in status
/// strategies remain the generic, untyped path. There is still no Group J
/// admission here — and the same
/// `subresources.status`-must-be-declared gate for a CRD-defined
/// resource (`update_status`'s own doc comment covers why). `dry_run` keeps
/// the same validation path while skipping the write.
pub async fn patch_status(storage: &mut StorageClient, group: &str, version: &str, resource: &str, namespace: Option<&str>, name: &str, kind_of_patch: PatchKind, patch_doc: &Value, dry_run: bool) -> Result<UpdateOutcome, Error> {
    patch_status_with_manager(storage, group, version, resource, namespace, name, kind_of_patch, patch_doc, dry_run, None).await
}

/// [`patch_status`] with the request's field manager.
pub async fn patch_status_with_manager(
    storage: &mut StorageClient,
    group: &str,
    version: &str,
    resource: &str,
    namespace: Option<&str>,
    name: &str,
    kind_of_patch: PatchKind,
    patch_doc: &Value,
    dry_run: bool,
    field_manager: Option<&str>,
) -> Result<UpdateOutcome, Error> {
    let Some(resolved) = resolve_resource(storage, group, version, resource).await? else {
        return Ok(UpdateOutcome::UnknownResource);
    };
    if !resolved.has_status_subresource {
        return Ok(UpdateOutcome::UnknownResource);
    }

    let key = keys::object_key(group, resource, namespace, name);
    let existing_resp = storage.range(RangeRequest { key: key.clone().into_bytes(), ..Default::default() }).await?;
    let Some(existing_kv) = existing_resp.kvs.into_iter().next() else {
        return Ok(UpdateOutcome::ObjectNotFound);
    };
    let existing_object = decrypt_and_decode_with_rotation(storage, group, resource, &existing_kv.key, &existing_kv.value, existing_kv.mod_revision).await?;
    let existing_object_for_request = convert_to_requested_version(storage, group, version, &resolved.kind, resolved.conversion_webhook.as_ref(), existing_object.clone()).await?;

    let patched = match apply_patch(kind_of_patch, resolved.schema, resolved.open_api_schema.as_ref(), &existing_object_for_request, patch_doc) {
        Ok(object) => object,
        Err(msg) => return Ok(UpdateOutcome::Invalid(vec![msg])),
    };

    let mut object = existing_object_for_request;
    match patched.get("status") {
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

    persist_update(storage, resolved.schema, resolved.open_api_schema.as_ref(), resolved.storage_open_api_schema.as_ref(), &resolved.kind, group, version, resource, key, &existing_kv, &existing_object, namespace, object, dry_run, resolved.conversion_webhook.clone(), field_manager, "status", false).await
}

/// Convenience wrapper combining [`patch_prepare`] and [`patch_persist`]
/// with no admission step in between — what `server::rest::patch` used
/// to do as one function before the split; kept for any caller that
/// doesn't need to run admission in the middle (this crate's own tests).
pub async fn patch(storage: &mut StorageClient, group: &str, version: &str, resource: &str, namespace: Option<&str>, name: &str, kind_of_patch: PatchKind, patch_doc: &Value) -> Result<UpdateOutcome, Error> {
    match patch_prepare(storage, group, version, resource, namespace, name, kind_of_patch, patch_doc).await? {
        PatchPrepareOutcome::Ready(candidate, context) => patch_persist(storage, group, version, resource, namespace, name, context, candidate, false).await,
        PatchPrepareOutcome::UnknownResource => Ok(UpdateOutcome::UnknownResource),
        PatchPrepareOutcome::ObjectNotFound => Ok(UpdateOutcome::ObjectNotFound),
        PatchPrepareOutcome::Invalid(v) => Ok(UpdateOutcome::Invalid(v)),
    }
}

/// The outcome of a Server-Side Apply request ([`server_side_apply`]).
#[derive(Debug, PartialEq)]
pub enum ApplyOutcome {
    /// The object as written, `metadata.managedFields` rebuilt to
    /// reflect this apply.
    Applied(Value),
    /// The merged-and-pruned result was byte-for-byte identical to the
    /// object already stored — nothing written, real upstream's own
    /// no-op contract (`crate::patch::updater::Applied::object`'s own
    /// doc comment). The caller still gets a real `200` with the
    /// current object, matching real upstream's own behavior.
    NoOp(Value),
    UnknownResource,
    /// No usable compiled or runtime structural schema was available for
    /// the resolved resource. Validated CRDs normally carry the latter;
    /// this remains a defensive outcome for malformed or legacy CRD data.
    UnsupportedForCrd,
    /// Another manager owns a field this apply is changing — real
    /// upstream's own `409 Conflict`, not raised unless `force` is
    /// false.
    Conflict(Vec<crate::patch::updater::Conflict>),
    Invalid(Vec<String>),
}

/// Server-Side Apply (`PATCH` with `Content-Type: application/apply-
/// patch+yaml`) — real upstream's `merge.Updater.Apply`, wired to real
/// storage (`crate::patch::updater::apply`,
/// `crate::patch::managed_fields`). `config` is the apply configuration,
/// already decoded from the request body by the caller (YAML or JSON —
/// real upstream accepts either for this content type, and this crate's
/// existing content negotiation already handles both for every other
/// verb).
///
/// Handles both real cases: an already-existing object (reads its
/// stored `managedFields`, runs `updater::apply` against it, persists
/// with the same optimistic-concurrency `Txn` every other write verb
/// uses) and **create-on-apply** (no object exists at this key yet —
/// real upstream's own Apply can create one, `liveObject` starting
/// empty; this branch runs the identical `updater::apply` orchestration
/// against an empty `live`, then persists with the same
/// create-only-if-absent `Txn` idiom `create`'s own doc comment names,
/// rather than `persist_update`'s update-if-matches one).
///
/// Named `server_side_apply`, not `apply_patch` — that name is already
/// this module's own private helper for the three ordinary patch kinds
/// (`json_patch`/`merge_patch`/`strategic_merge`) just above; this is a
/// wholly different real orchestration, not a fourth branch of that one.
///
/// A convenience wrapper combining [`apply_prepare`] and
/// [`apply_persist`] with no admission step in between — the same shape
/// [`patch`] is to [`patch_prepare`]/[`patch_persist`]. `server::
/// listener`'s own real request handler calls the two halves directly
/// instead, so it can run Group J's `LimitRanger` PVC check against the
/// real candidate object in between, the same way it already does for
/// the three-patch-kind `PATCH` path.
///
/// A CRD-defined resource with an established structural schema uses the
/// runtime-schema SSA path; malformed or schema-less CRD records retain the
/// defensive [`ApplyOutcome::UnsupportedForCrd`] outcome.
pub async fn server_side_apply(storage: &mut StorageClient, group: &str, version: &str, resource: &str, namespace: Option<&str>, name: &str, manager: &str, force: bool, config: &Value) -> Result<ApplyOutcome, Error> {
    match apply_prepare(storage, group, version, resource, namespace, name, manager, force, config).await? {
        ApplyPrepareOutcome::Ready(candidate, context) => apply_persist(storage, group, version, resource, namespace, context, candidate, false).await,
        ApplyPrepareOutcome::UnknownResource => Ok(ApplyOutcome::UnknownResource),
        ApplyPrepareOutcome::Conflict(c) => Ok(ApplyOutcome::Conflict(c)),
        ApplyPrepareOutcome::Invalid(v) => Ok(ApplyOutcome::Invalid(v)),
        ApplyPrepareOutcome::NoOp(v) => Ok(ApplyOutcome::NoOp(v)),
        ApplyPrepareOutcome::UnsupportedForCrd => Ok(ApplyOutcome::UnsupportedForCrd),
    }
}

/// The context [`apply_prepare`] hands back to [`apply_persist`] once the
/// merged, pruned, conflict-checked, validated, defaulted candidate is
/// ready — enough for a caller (`server::listener`) to run Group J
/// admission (`LimitRanger`'s own PVC check) against the real candidate
/// in between, the same split [`PatchContext`] already exists for.
#[derive(Debug)]
pub struct ApplyContext {
    /// `Some` for a built-in compiled schema and `None` for a CRD whose
    /// runtime schema has already been consumed during preparation.
    schema: Option<&'static str>,
    storage_open_api_schema: Option<Value>,
    kind: String,
    conversion_webhook: Option<apiextensions::registry::ConversionWebhook>,
    key: String,
    /// `Some((existing_kv, live))` for an update-on-apply (persisted via
    /// [`persist_update`]'s update-if-matches `Txn`); `None` for
    /// create-on-apply (persisted via the same create-only-if-absent
    /// `Txn` idiom [`create`]'s own doc comment names).
    existing: Option<(mvccpb::KeyValue, Value)>,
}

#[derive(Debug)]
pub enum ApplyPrepareOutcome {
    Ready(Value, ApplyContext),
    UnknownResource,
    Conflict(Vec<crate::patch::updater::Conflict>),
    Invalid(Vec<String>),
    /// No usable compiled or runtime structural schema was available for
    /// the resolved resource. Established CRDs normally carry the latter;
    /// this remains a defensive outcome for malformed or legacy CRD data.
    UnsupportedForCrd,
    /// The merged-and-pruned result was identical to what's already
    /// stored (or, for create-on-apply, `config` was itself empty) —
    /// nothing to persist, `Value` is what to return to the caller.
    NoOp(Value),
}
