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
/// Apply's own `application/apply-patch+yaml` isn't recognized — Group
/// G's own doc comment already names SSA/managedFields as not yet
/// landed.
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
    if is_crd {
        PatchKind::Merge
    } else {
        PatchKind::StrategicMerge
    }
}

/// Resolves the resource and returns the default patch strategy for a
/// request with no `Content-Type`. `None` means the URL names no resource
/// this server knows about, so the listener can preserve its normal 404
/// response rather than reporting a media-type error.
pub async fn default_patch_kind_for_request(
    storage: &mut StorageClient,
    group: &str,
    version: &str,
    resource: &str,
) -> Result<Option<PatchKind>, Error> {
    Ok(resolve_resource(storage, group, version, resource)
        .await?
        .map(|resolved| default_patch_kind(resolved.schema.is_none())))
}

/// Apply a CEL `MutatingAdmissionPolicy` apply configuration to an admission
/// object. Apply configurations use the same strategic-merge rules as the
/// server's strategic-merge PATCH path; built-ins use their generated schema
/// and CRDs use their runtime OpenAPI schema. A resource without either
/// schema falls back to JSON merge semantics, which preserves the generic
/// server's behavior for schema-less resources.
pub async fn apply_admission_configuration(
    storage: &mut StorageClient,
    group: &str,
    version: &str,
    resource: &str,
    existing: &Value,
    configuration: &Value,
) -> Result<Value, Error> {
    let Some(resolved) = resolve_resource(storage, group, version, resource).await? else {
        return Err(Error::UnknownResource);
    };
    Ok(match (resolved.schema, resolved.open_api_schema.as_ref()) {
        (Some(schema), _) => crate::patch::strategic_merge::apply(schema, existing, configuration),
        (None, Some(schema)) => {
            apiextensions::schema_strategic_merge::apply(schema, existing, configuration)
        }
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
    kind: String,
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
fn apply_patch(
    kind_of_patch: PatchKind,
    schema: Option<&str>,
    open_api_schema: Option<&Value>,
    existing: &Value,
    patch_doc: &Value,
) -> Result<Value, String> {
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
pub async fn patch_prepare(
    storage: &mut StorageClient,
    group: &str,
    version: &str,
    resource: &str,
    namespace: Option<&str>,
    name: &str,
    kind_of_patch: PatchKind,
    patch_doc: &Value,
) -> Result<PatchPrepareOutcome, Error> {
    let Some(resolved) = resolve_resource(storage, group, version, resource).await? else {
        return Ok(PatchPrepareOutcome::UnknownResource);
    };

    let key = keys::object_key(group, resource, namespace, name);
    let existing_resp = storage
        .range(RangeRequest {
            key: key.clone().into_bytes(),
            ..Default::default()
        })
        .await?;
    let Some(existing_kv) = existing_resp.kvs.into_iter().next() else {
        return Ok(PatchPrepareOutcome::ObjectNotFound);
    };
    let existing_object = decrypt_and_decode(
        storage,
        group,
        resource,
        &existing_kv.key,
        &existing_kv.value,
    )?;

    let patched = match apply_patch(
        kind_of_patch,
        resolved.schema,
        resolved.open_api_schema.as_ref(),
        &existing_object,
        patch_doc,
    ) {
        Ok(object) => object,
        Err(msg) => return Ok(PatchPrepareOutcome::Invalid(vec![msg])),
    };

    Ok(PatchPrepareOutcome::Ready(
        patched,
        PatchContext {
            schema: resolved.schema,
            open_api_schema: resolved.open_api_schema,
            kind: resolved.kind,
            key,
            existing_kv,
            existing_object,
        },
    ))
}

/// The "persist" half of [`patch`]: validates/defaults `candidate` (the
/// object [`patch_prepare`] produced, possibly further mutated by
/// admission in between) and writes it with the same real optimistic
/// concurrency [`update`] uses (`Txn`-compared-against-`ModRevision`,
/// via the shared [`persist_update`] tail) — no client-submitted
/// `resourceVersion` needed, since the object being patched *is* the one
/// [`patch_prepare`] already read.
pub async fn patch_persist(
    storage: &mut StorageClient,
    group: &str,
    version: &str,
    resource: &str,
    namespace: Option<&str>,
    name: &str,
    context: PatchContext,
    candidate: Value,
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
            let mut v: Vec<String> = validation::validate_required(schema, &candidate)
                .into_iter()
                .map(|m| format!("{}: Required value", m.path))
                .collect();
            v.extend(
                validation::validate_types(schema, &candidate)
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
                apiextensions::schema_validation::validate_required(open_api_schema, &candidate)
                    .into_iter()
                    .map(|m| format!("{}: Required value", m.path))
                    .collect();
            v.extend(
                apiextensions::schema_validation::validate_types(open_api_schema, &candidate)
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
                &candidate,
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
    if !violations.is_empty() {
        return Ok(UpdateOutcome::Invalid(violations));
    }

    let object = match (context.schema, &context.open_api_schema) {
        (Some(schema), _) => defaulting::apply_defaults(schema, &candidate),
        (None, Some(open_api_schema)) => {
            apiextensions::schema_defaults::apply_defaults(open_api_schema, &candidate)
        }
        (None, None) => candidate,
    };

    // CEL Phase 4: same real rule evaluation `create`/`update` both run.
    if let Some(open_api_schema) = &context.open_api_schema {
        let rule_violations = apiextensions::cel_evaluate::validate_object(
            open_api_schema,
            &object,
            Some(&context.existing_object),
        );
        if !rule_violations.is_empty() {
            return Ok(UpdateOutcome::Invalid(
                rule_violations.into_iter().map(|v| v.to_string()).collect(),
            ));
        }
    }

    persist_update(
        storage,
        context.schema,
        &context.kind,
        group,
        version,
        resource,
        context.key,
        &context.existing_kv,
        &context.existing_object,
        namespace,
        object,
        false,
    )
    .await
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
/// resource (`update_status`'s own doc comment covers why).
pub async fn patch_status(
    storage: &mut StorageClient,
    group: &str,
    version: &str,
    resource: &str,
    namespace: Option<&str>,
    name: &str,
    kind_of_patch: PatchKind,
    patch_doc: &Value,
) -> Result<UpdateOutcome, Error> {
    let Some(resolved) = resolve_resource(storage, group, version, resource).await? else {
        return Ok(UpdateOutcome::UnknownResource);
    };
    if !resolved.has_status_subresource {
        return Ok(UpdateOutcome::UnknownResource);
    }

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

    let patched = match apply_patch(
        kind_of_patch,
        resolved.schema,
        resolved.open_api_schema.as_ref(),
        &existing_object,
        patch_doc,
    ) {
        Ok(object) => object,
        Err(msg) => return Ok(UpdateOutcome::Invalid(vec![msg])),
    };

    let mut object = existing_object.clone();
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

    persist_update(
        storage,
        resolved.schema,
        &resolved.kind,
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

/// Convenience wrapper combining [`patch_prepare`] and [`patch_persist`]
/// with no admission step in between — what `server::rest::patch` used
/// to do as one function before the split; kept for any caller that
/// doesn't need to run admission in the middle (this crate's own tests).
pub async fn patch(
    storage: &mut StorageClient,
    group: &str,
    version: &str,
    resource: &str,
    namespace: Option<&str>,
    name: &str,
    kind_of_patch: PatchKind,
    patch_doc: &Value,
) -> Result<UpdateOutcome, Error> {
    match patch_prepare(
        storage,
        group,
        version,
        resource,
        namespace,
        name,
        kind_of_patch,
        patch_doc,
    )
    .await?
    {
        PatchPrepareOutcome::Ready(candidate, context) => {
            patch_persist(
                storage, group, version, resource, namespace, name, context, candidate,
            )
            .await
        }
        PatchPrepareOutcome::UnknownResource => Ok(UpdateOutcome::UnknownResource),
        PatchPrepareOutcome::ObjectNotFound => Ok(UpdateOutcome::ObjectNotFound),
        PatchPrepareOutcome::Invalid(v) => Ok(UpdateOutcome::Invalid(v)),
    }
}
