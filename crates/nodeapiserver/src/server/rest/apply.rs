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
pub async fn server_side_apply(
    storage: &mut StorageClient,
    group: &str,
    version: &str,
    resource: &str,
    namespace: Option<&str>,
    name: &str,
    manager: &str,
    force: bool,
    config: &Value,
) -> Result<ApplyOutcome, Error> {
    match apply_prepare(
        storage, group, version, resource, namespace, name, manager, force, config,
    )
    .await?
    {
        ApplyPrepareOutcome::Ready(candidate, context) => {
            apply_persist(
                storage, group, version, resource, namespace, context, candidate, false,
            )
            .await
        }
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
    has_status_subresource: bool,
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

fn managed_field_key(manager: &str, subresource: &str) -> String {
    if subresource.is_empty() {
        manager.to_string()
    } else {
        format!("{manager}/{subresource}")
    }
}

fn managers_for_request(
    managers: &crate::patch::managed_fields::VersionedManagers,
    request_api_version: &str,
) -> BTreeMap<String, crate::patch::fieldset::Set> {
    managers
        .iter()
        .filter(|(_, state)| state.api_version == request_api_version)
        .map(|(manager, state)| (manager.clone(), state.fields.clone()))
        .collect()
}

#[derive(Debug, Clone)]
enum ManagedFieldSchema {
    Builtin(&'static str),
    Crd(Value),
}

async fn managed_field_schema(
    storage: &mut StorageClient,
    group: &str,
    request_api_version: &str,
    request_open_api_schema: Option<&Value>,
    request_schema: Option<&str>,
    resource: &str,
    kind: &str,
    api_version: &str,
) -> Result<Option<ManagedFieldSchema>, Error> {
    let (version_group, version) = split_api_version(api_version);
    let version_group = if version_group.is_empty() {
        group
    } else {
        version_group
    };
    if request_schema.is_some() {
        return Ok(
            protobuf::schema_for_gvk(version_group, version, kind).map(ManagedFieldSchema::Builtin)
        );
    }
    let schema = if api_version == request_api_version {
        request_open_api_schema.cloned()
    } else {
        resolve_resource(storage, version_group, version, resource)
            .await?
            .and_then(|resolved| resolved.open_api_schema)
    };
    Ok(schema.map(ManagedFieldSchema::Crd))
}

fn managed_field_set(schema: &ManagedFieldSchema, object: &Value) -> crate::patch::fieldset::Set {
    match schema {
        ManagedFieldSchema::Builtin(schema) => {
            crate::patch::fieldset::set_from_object(schema, object)
        }
        ManagedFieldSchema::Crd(schema) => crate::patch::crd_apply::set_from_object(schema, object),
    }
}

fn managed_field_members(
    schema: &ManagedFieldSchema,
    fields: &crate::patch::fieldset::Set,
) -> crate::patch::fieldset::Set {
    match schema {
        ManagedFieldSchema::Builtin(schema) => {
            crate::patch::fieldset::ensure_named_fields_are_members(schema, fields)
        }
        ManagedFieldSchema::Crd(_) => fields.clone(),
    }
}

fn managed_field_remove(
    schema: &ManagedFieldSchema,
    object: &Value,
    fields: &crate::patch::fieldset::Set,
) -> Value {
    match schema {
        ManagedFieldSchema::Builtin(schema) => {
            let fields = crate::patch::fieldset::ensure_named_fields_are_members(schema, fields);
            crate::patch::fieldset::remove_items(schema, object, &fields)
        }
        ManagedFieldSchema::Crd(schema) => {
            crate::patch::crd_apply::remove_items(schema, object, fields)
        }
    }
}

/// Reconcile each stored CRD manager's field set with the schema for the API
/// version that recorded it. Upstream performs this before both Update and
/// Apply so an atomic/granular CRD schema change cannot leave ownership paths
/// interpreted under the old relationship.
async fn reconcile_versioned_managers_with_schema(
    storage: &mut StorageClient,
    group: &str,
    request_api_version: &str,
    request_open_api_schema: Option<&Value>,
    request_schema: Option<&str>,
    resource: &str,
    kind: &str,
    managers: &crate::patch::managed_fields::VersionedManagers,
) -> Result<crate::patch::managed_fields::VersionedManagers, Error> {
    let mut reconciled = BTreeMap::new();
    for (manager, state) in managers {
        let Some(schema) = managed_field_schema(
            storage,
            group,
            request_api_version,
            request_open_api_schema,
            request_schema,
            resource,
            kind,
            &state.api_version,
        )
        .await?
        else {
            // An API version that is no longer served is obsolete managed
            // field data; upstream drops it during reconciliation.
            continue;
        };
        let fields = match schema {
            ManagedFieldSchema::Builtin(_) => state.fields.clone(),
            ManagedFieldSchema::Crd(schema) => {
                crate::apiextensions::schema_apply::reconcile_field_set_with_schema(
                    &schema,
                    &state.fields,
                )
            }
        };
        reconciled.insert(
            manager.clone(),
            crate::patch::managed_fields::VersionedManager {
                fields,
                api_version: state.api_version.clone(),
                applied: state.applied,
            },
        );
    }
    Ok(reconciled)
}

/// Performs Apply's version-aware prune step. The request-schema updater can
/// only prune a manager's previous fields when that manager used the current
/// request version; upstream instead converts the merged candidate into the
/// previous Apply version, removes the fields no longer configured, adds back
/// fields still owned at any version, and converts the result forward again.
async fn prune_versioned_apply(
    storage: &mut StorageClient,
    group: &str,
    request_version: &str,
    resource: &str,
    kind: &str,
    request_schema: Option<&str>,
    request_open_api_schema: Option<&Value>,
    conversion_webhook: Option<&apiextensions::registry::ConversionWebhook>,
    candidate: Value,
    managers: &crate::patch::managed_fields::VersionedManagers,
    manager: &str,
    last_state: &crate::patch::managed_fields::VersionedManager,
    request_fields: &crate::patch::fieldset::Set,
) -> Result<Value, Error> {
    if last_state.fields.is_empty() {
        return Ok(candidate);
    }
    let request_api_version = if group.is_empty() {
        request_version.to_string()
    } else {
        format!("{group}/{request_version}")
    };
    let last_api_version = if last_state.api_version.is_empty() {
        request_api_version.clone()
    } else {
        last_state.api_version.clone()
    };
    let Some(last_schema) = managed_field_schema(
        storage,
        group,
        &request_api_version,
        request_open_api_schema,
        request_schema,
        resource,
        kind,
        &last_api_version,
    )
    .await?
    else {
        // An obsolete previous version has the same safe behavior as
        // upstream's missing-version converter: retain the merged object.
        return Ok(candidate);
    };
    let (_, last_version) = split_api_version(&last_api_version);
    let mut merged = convert_to_requested_version(
        storage,
        group,
        last_version,
        kind,
        conversion_webhook,
        candidate,
    )
    .await?;
    let mut pruned = managed_field_remove(&last_schema, &merged, &last_state.fields);

    let mut all_managers = managers.clone();
    all_managers.insert(
        manager.to_string(),
        crate::patch::managed_fields::VersionedManager {
            fields: request_fields.clone(),
            api_version: request_api_version.clone(),
            applied: true,
        },
    );
    let mut fields_by_version = BTreeMap::<String, crate::patch::fieldset::Set>::new();
    for state in all_managers.values() {
        let entry = fields_by_version
            .entry(state.api_version.clone())
            .or_default();
        *entry = entry.union(&state.fields);
    }

    let mut versions = vec![last_api_version.clone()];
    for api_version in fields_by_version.keys() {
        if api_version != &last_api_version {
            versions.push(api_version.clone());
        }
    }
    for api_version in versions {
        let Some(schema) = managed_field_schema(
            storage,
            group,
            &request_api_version,
            request_open_api_schema,
            request_schema,
            resource,
            kind,
            &api_version,
        )
        .await?
        else {
            continue;
        };
        let (_, version) = split_api_version(&api_version);
        merged =
            convert_to_requested_version(storage, group, version, kind, conversion_webhook, merged)
                .await?;
        pruned =
            convert_to_requested_version(storage, group, version, kind, conversion_webhook, pruned)
                .await?;
        let merged_fields = managed_field_set(&schema, &merged);
        let pruned_fields = managed_field_set(&schema, &pruned);
        let managed = fields_by_version
            .get(&api_version)
            .cloned()
            .unwrap_or_default();
        let to_remove = merged_fields.difference(&pruned_fields.union(&managed));
        pruned = managed_field_remove(&schema, &merged, &to_remove);
    }

    merged = convert_to_requested_version(
        storage,
        group,
        last_version,
        kind,
        conversion_webhook,
        merged,
    )
    .await?;
    pruned = convert_to_requested_version(
        storage,
        group,
        last_version,
        kind,
        conversion_webhook,
        pruned,
    )
    .await?;
    let merged_fields = managed_field_set(&last_schema, &merged);
    let pruned_fields = managed_field_set(&last_schema, &pruned);
    let last_fields = managed_field_members(&last_schema, &last_state.fields);
    let dangling = merged_fields
        .difference(&pruned_fields)
        .intersection(&last_fields);
    pruned = managed_field_remove(&last_schema, &merged, &dangling);
    convert_to_requested_version(
        storage,
        group,
        request_version,
        kind,
        conversion_webhook,
        pruned,
    )
    .await
}

/// Compare the live object and Apply candidate in each manager's recorded
/// version. Upstream's managed fields are versioned because a field path is
/// only meaningful under the schema that produced it; for example, HPA v1's
/// `targetCPUUtilizationPercentage` is represented as v2's `metrics` list.
async fn compare_managed_fields_in_recorded_versions(
    storage: &mut StorageClient,
    group: &str,
    version: &str,
    resource: &str,
    kind: &str,
    schema: Option<&str>,
    open_api_schema: Option<&Value>,
    conversion_webhook: Option<&apiextensions::registry::ConversionWebhook>,
    live: &Value,
    live_for_request: &Value,
    candidate: &Value,
    entries: &[crate::patch::managed_fields::ManagedFieldsEntry],
) -> Result<BTreeMap<String, crate::patch::typed_compare::Comparison>, Error> {
    let request_api_version = if group.is_empty() {
        version.to_string()
    } else {
        format!("{group}/{version}")
    };
    let mut comparisons = BTreeMap::new();

    for entry in entries {
        let key = managed_field_key(&entry.manager, &entry.subresource);
        let manager_api_version = if entry.api_version.is_empty() {
            request_api_version.clone()
        } else {
            entry.api_version.clone()
        };
        let (manager_group, manager_version) = split_api_version(&manager_api_version);
        let (old, new) = if manager_api_version == request_api_version {
            (live_for_request.clone(), candidate.clone())
        } else {
            let old = convert_to_requested_version(
                storage,
                group,
                manager_version,
                kind,
                conversion_webhook,
                live.clone(),
            )
            .await?;
            let new = convert_to_requested_version(
                storage,
                group,
                manager_version,
                kind,
                conversion_webhook,
                candidate.clone(),
            )
            .await?;
            (old, new)
        };

        let comparison = if let Some(_) = schema {
            let Some(manager_schema) =
                protobuf::schema_for_gvk(manager_group, manager_version, kind)
            else {
                // Real upstream drops managed-field entries for versions it
                // can no longer serve. Leave it out of the comparison map;
                // the versioned reconciler performs that same cleanup.
                continue;
            };
            crate::patch::typed_compare::compare(manager_schema, &old, &new)
        } else {
            let manager_schema = if manager_api_version == request_api_version {
                open_api_schema.cloned()
            } else {
                resolve_resource(storage, manager_group, manager_version, resource)
                    .await?
                    .and_then(|resolved| resolved.open_api_schema)
            };
            let Some(manager_schema) = manager_schema else {
                continue;
            };
            crate::patch::crd_apply::compare_for_managed_fields(&manager_schema, &old, &new)
        };
        comparisons.insert(key, comparison);
    }

    Ok(comparisons)
}

include!("apply_prepare.rs");
