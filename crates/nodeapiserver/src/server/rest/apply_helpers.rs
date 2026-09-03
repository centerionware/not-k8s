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
pub async fn apply_persist(
    storage: &mut StorageClient,
    group: &str,
    version: &str,
    resource: &str,
    namespace: Option<&str>,
    context: ApplyContext,
    mut object: Value,
    dry_run: bool,
) -> Result<ApplyOutcome, Error> {
    let Some((existing_kv, live)) = context.existing else {
        object = convert_to_storage_version(
            storage,
            group,
            version,
            context.conversion_webhook.as_ref(),
            object,
        )
        .await?;
        object = match revalidate_storage_object(context.storage_open_api_schema.as_ref(), object) {
            Ok(object) => object,
            Err(violations) => return Ok(ApplyOutcome::Invalid(violations)),
        };
        if dry_run {
            let object = convert_to_requested_version(
                storage,
                group,
                version,
                &context.kind,
                context.conversion_webhook.as_ref(),
                object,
            )
            .await?;
            return Ok(ApplyOutcome::Applied(object));
        }
        let stored_version = context
            .conversion_webhook
            .as_ref()
            .map_or(version, |conversion| conversion.storage_version.as_str());
        let api_version = if group.is_empty() {
            stored_version.to_string()
        } else {
            format!("{group}/{stored_version}")
        };
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
        let envelope =
            encrypt_for_storage(storage, group, resource, context.key.as_bytes(), &envelope)?;
        let put = pb::PutRequest {
            key: context.key.into_bytes(),
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
            // Lost the race: something else created this key between
            // `apply_prepare`'s own read and this write.
            return Ok(ApplyOutcome::Conflict(Vec::new()));
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
            &context.kind,
            context.conversion_webhook.as_ref(),
            object,
        )
        .await?;
        return Ok(ApplyOutcome::Applied(object));
    };

    match persist_update(
        storage,
        context.schema,
        None,
        context.storage_open_api_schema.as_ref(),
        &context.kind,
        group,
        version,
        resource,
        context.key,
        &existing_kv,
        &live,
        namespace,
        object,
        dry_run,
        context.conversion_webhook,
        None,
        "",
        context.has_status_subresource,
        true,
    )
    .await?
    {
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
        other => unreachable!(
            "persist_update only ever returns Updated or Conflict for an already-decoded, already-validated object: {other:?}"
        ),
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
        ("scheduling.k8s.io", "priorityclasses") => {
            crate::scheme::name_format::is_dns1123_subdomain(name)
        }
        ("resource.k8s.io", "resourceclaims") => {
            crate::scheme::name_format::is_dns1123_subdomain(name)
        }
        ("resource.k8s.io", "resourceclaimtemplates") => {
            crate::scheme::name_format::is_dns1123_subdomain(name)
        }
        ("storage.k8s.io", "storageclasses") => {
            crate::scheme::name_format::is_dns1123_subdomain(name)
        }
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
        // `apps/v1` StatefulSet names use the stricter DNS1123Label rule
        // because the name becomes part of generated Pod names
        // (`ValidateStatefulSetName`).
        // `autoscaling/v1` and `autoscaling/v2` HorizontalPodAutoscaler
        // names use the DNS-subdomain rule (`ValidateHorizontalPodAutoscalerName`).
        ("apps", "controllerrevisions") => crate::scheme::name_format::is_dns1123_subdomain(name),
        ("apps", "daemonsets") => crate::scheme::name_format::is_dns1123_subdomain(name),
        ("apps", "deployments") => crate::scheme::name_format::is_dns1123_subdomain(name),
        ("apps", "replicasets") => crate::scheme::name_format::is_dns1123_subdomain(name),
        ("apps", "statefulsets") => crate::scheme::name_format::is_dns1123_label(name),
        ("autoscaling", "horizontalpodautoscalers") => {
            crate::scheme::name_format::is_dns1123_subdomain(name)
        }
        ("networking.k8s.io", "ingresses") => {
            crate::scheme::name_format::is_dns1123_subdomain(name)
        }
        ("networking.k8s.io", "ingressclasses") => {
            crate::scheme::name_format::is_dns1123_subdomain(name)
        }
        ("networking.k8s.io", "servicecidrs") => {
            crate::scheme::name_format::is_dns1123_subdomain(name)
        }
        ("discovery.k8s.io", "endpointslices") => {
            crate::scheme::name_format::is_dns1123_subdomain(name)
        }
        ("flowcontrol.apiserver.k8s.io", "flowschemas") => {
            crate::scheme::name_format::is_dns1123_subdomain(name)
        }
        ("flowcontrol.apiserver.k8s.io", "prioritylevelconfigurations") => {
            crate::scheme::name_format::is_dns1123_subdomain(name)
        }
        ("node.k8s.io", "runtimeclasses") => crate::scheme::name_format::is_dns1123_subdomain(name),
        ("coordination.k8s.io", "leases") => crate::scheme::name_format::is_dns1123_subdomain(name),
        // `batch/v1` Job and CronJob names use upstream's
        // `ValidateReplicationControllerName` rule, which is
        // `NameIsDNSSubdomain`. The group/resource keys are also present in
        // the vendored discovery spec, so this does not apply the rule to an
        // unrelated resource with the same plural.
        ("batch", "jobs") => crate::scheme::name_format::is_dns1123_subdomain(name),
        ("batch", "cronjobs") => crate::scheme::name_format::is_dns1123_subdomain(name),
        // `events.k8s.io/v1` applies `NameIsDNSSubdomain` to Event metadata.
        // Core `events` and `events.k8s.io/v1beta1` retain legacy validation
        // behavior and intentionally remain outside this entry.
        ("events.k8s.io", "events") => crate::scheme::name_format::is_dns1123_subdomain(name),
        _ => Vec::new(),
    }
}

/// Validates metadata fields shared by every Kubernetes object. OpenAPI's
/// `additionalProperties` can validate map values but cannot constrain the
/// property names themselves, so labels, annotations, and finalizers need
/// this universal metadata pass in addition to the per-kind schema checks.
fn metadata_format_violations(object: &Value) -> Vec<String> {
    let Some(metadata) = object.get("metadata") else {
        return Vec::new();
    };
    let Some(metadata) = metadata.as_object() else {
        return vec!["metadata must be an object".to_string()];
    };
    let mut violations = Vec::new();

    for field in ["labels", "annotations"] {
        let Some(values) = metadata.get(field) else {
            continue;
        };
        let Some(values) = values.as_object() else {
            violations.push(format!("metadata.{field} must be an object"));
            continue;
        };
        for (key, value) in values {
            for error in crate::scheme::name_format::is_qualified_name(key) {
                violations.push(format!("metadata.{field}[{key:?}]: {error}"));
            }
            let Some(value) = value.as_str() else {
                violations.push(format!("metadata.{field}[{key:?}] must be a string"));
                continue;
            };
            if field == "labels" {
                for error in crate::scheme::name_format::is_label_value(value) {
                    violations.push(format!("metadata.labels[{key:?}]: {error}"));
                }
            }
        }
    }

    if let Some(finalizers) = metadata.get("finalizers") {
        let Some(finalizers) = finalizers.as_array() else {
            violations.push("metadata.finalizers must be an array".to_string());
            return violations;
        };
        for (index, finalizer) in finalizers.iter().enumerate() {
            let Some(finalizer) = finalizer.as_str() else {
                violations.push(format!("metadata.finalizers[{index}] must be a string"));
                continue;
            };
            for error in crate::scheme::name_format::is_qualified_name(finalizer) {
                violations.push(format!("metadata.finalizers[{index}]: {error}"));
            }
        }
    }
    violations
}

/// No-ops (rather than panicking, matching this crate's established
/// "malformed/adversarial input degrades gracefully" posture) if `object`
/// isn't itself a JSON object — `serde_json::Value`'s `IndexMut` panics
/// on a non-object, non-null receiver, and a request body that made it
/// this far without being an object at all is a real, if unlikely, case
/// (an empty-`required`-list schema lets `validate_required`/
/// `validate_types` both pass on one).
fn set_metadata_field(object: &mut Value, field: &str, value: Value) {
    let Some(map) = object.as_object_mut() else {
        return;
    };
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
        for field in [
            "resourceVersion",
            "creationTimestamp",
            "selfLink",
            "uid",
            "managedFields",
        ] {
            metadata.remove(field);
        }
    }
    object
}

fn ignored_managed_fields(
    has_status_subresource: bool,
    subresource: &str,
) -> crate::patch::fieldset::Set {
    let mut ignored = crate::patch::fieldset::Set::new();
    if has_status_subresource && subresource.is_empty() {
        ignored.insert(&[crate::patch::fieldset::PathElement::Field(
            "status".to_string(),
        )]);
    }
    ignored
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
    has_status_subresource: bool,
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
        return reconcile_runtime_managed_fields(
            manager_schema,
            existing,
            object,
            manager,
            operation,
            subresource,
            group,
            version,
            has_status_subresource,
        );
    };

    let previous = existing
        .pointer("/metadata/managedFields")
        .cloned()
        .unwrap_or_else(|| Value::Array(Vec::new()));
    let entries = crate::patch::managed_fields::parse_managed_fields(&previous).unwrap_or_default();
    let managers = crate::patch::managed_fields::to_managers_map(&entries);
    let live = strip_managed_field_system_fields(existing.clone());
    let new = strip_managed_field_system_fields(object.clone());
    let manager_key = if subresource.is_empty() {
        manager.to_string()
    } else {
        format!("{manager}/{subresource}")
    };
    let ignored = ignored_managed_fields(has_status_subresource, subresource);
    let managers = crate::patch::updater::apply_update_with_ignored_fields(
        manager_schema,
        &live,
        &new,
        &managers,
        &manager_key,
        Some(&ignored),
    );
    let api_version = if group.is_empty() {
        version.to_string()
    } else {
        format!("{group}/{version}")
    };
    let rebuilt = crate::patch::managed_fields::rebuild_managed_fields(
        &entries,
        &managers,
        manager,
        subresource,
        operation,
        &api_version,
        Some(&now_rfc3339()),
    );
    set_metadata_field(
        &mut object,
        "managedFields",
        crate::patch::managed_fields::render_managed_fields(&rebuilt),
    );
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
    has_status_subresource: bool,
) -> Value {
    let previous = existing
        .pointer("/metadata/managedFields")
        .cloned()
        .unwrap_or_else(|| Value::Array(Vec::new()));
    let entries = crate::patch::managed_fields::parse_managed_fields(&previous).unwrap_or_default();
    let managers = crate::patch::managed_fields::to_managers_map(&entries);
    let live = strip_managed_field_system_fields(existing.clone());
    let new = strip_managed_field_system_fields(object.clone());
    let manager_key = if subresource.is_empty() {
        manager.to_string()
    } else {
        format!("{manager}/{subresource}")
    };
    let managers =
        crate::apiextensions::schema_apply::reconcile_managed_fields_with_schema(schema, &managers);
    let ignored = ignored_managed_fields(has_status_subresource, subresource);
    let managers = crate::apiextensions::schema_apply::apply_update_with_ignored_fields(
        schema,
        &live,
        &new,
        &managers,
        &manager_key,
        Some(&ignored),
    );
    let api_version = if group.is_empty() {
        version.to_string()
    } else {
        format!("{group}/{version}")
    };
    let rebuilt = crate::patch::managed_fields::rebuild_managed_fields(
        &entries,
        &managers,
        manager,
        subresource,
        operation,
        &api_version,
        Some(&now_rfc3339()),
    );
    set_metadata_field(
        &mut object,
        "managedFields",
        crate::patch::managed_fields::render_managed_fields(&rebuilt),
    );
    object
}

/// Live bug found verifying #541's fix: `Namespace` is the one core type
/// whose finalizers live at `spec.finalizers`, not the standard
/// `metadata.finalizers` every other resource uses -- real upstream's own
/// `NamespaceSpec.Finalizers` field, ported faithfully by `scheme/
/// defaulting.rs`'s `default_namespace()`, which stamps exactly that path.
/// Checking only `metadata/finalizers` meant a Namespace's own finalizer
/// was never recognized here at all: `delete_with_options()` deleted it
/// immediately regardless, defeating #541's fix even though the finalizer
/// really was present on the object -- confirmed live, a DELETE response
/// for a Namespace with `spec.finalizers: ["kubernetes"]` came back with
/// no `deletionTimestamp` and `status.phase` still `"Active"`, deleted
/// outright. Checking `spec/finalizers` unconditionally for every kind is
/// safe: no other built-in type defines that path, so it's simply absent
/// (and this check already treats absent as "no finalizers") for
/// everything but Namespace.
fn has_finalizers(object: &Value) -> bool {
    let list_has_entries = |pointer: &str| {
        object
            .pointer(pointer)
            .and_then(Value::as_array)
            .is_some_and(|finalizers| !finalizers.is_empty())
    };
    list_has_entries("/metadata/finalizers") || list_has_entries("/spec/finalizers")
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
    let Some(map) = object.as_object_mut() else {
        return;
    };
    map.insert("kind".to_string(), Value::String(kind.to_string()));
    map.insert(
        "apiVersion".to_string(),
        Value::String(api_version.to_string()),
    );
}

/// Second-precision RFC3339 with a `Z` suffix (`"2026-08-20T09:30:00Z"`)
/// — matches real upstream's own `metav1.Time` marshaling, which never
/// carries sub-second precision.
fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

#[cfg(test)]
mod has_finalizers_tests {
    use super::has_finalizers;
    use serde_json::json;

    #[test]
    fn an_object_with_no_finalizers_anywhere_has_none() {
        assert!(!has_finalizers(&json!({"metadata": {}, "spec": {}})));
    }

    #[test]
    fn metadata_finalizers_is_recognized_for_ordinary_resources() {
        assert!(has_finalizers(&json!({"metadata": {"finalizers": ["kubernetes.io/pv-protection"]}})));
    }

    #[test]
    fn an_empty_metadata_finalizers_list_is_not_a_finalizer() {
        assert!(!has_finalizers(&json!({"metadata": {"finalizers": []}})));
    }

    #[test]
    fn namespace_spec_finalizers_is_recognized() {
        // Live bug found verifying #541: Namespace stores its finalizer at
        // spec.finalizers, not metadata.finalizers -- a DELETE for a
        // Namespace with only this field set must still defer.
        assert!(has_finalizers(&json!({"metadata": {}, "spec": {"finalizers": ["kubernetes"]}})));
    }

    #[test]
    fn an_empty_spec_finalizers_list_is_not_a_finalizer() {
        assert!(!has_finalizers(&json!({"metadata": {}, "spec": {"finalizers": []}})));
    }
}
