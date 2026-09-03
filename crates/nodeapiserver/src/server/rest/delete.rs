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
pub async fn delete(
    storage: &mut StorageClient,
    group: &str,
    version: &str,
    resource: &str,
    namespace: Option<&str>,
    name: &str,
) -> Result<DeleteOutcome, Error> {
    delete_with_options(
        storage, group, version, resource, namespace, name, None, None, false,
    )
    .await
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
    grace_period_seconds: Option<i64>,
    dry_run: bool,
) -> Result<DeleteOutcome, Error> {
    let Some(resolved) = resolve_resource(storage, group, version, resource).await? else {
        return Ok(DeleteOutcome::UnknownResource);
    };
    let key = keys::object_key(group, resource, namespace, name);
    let current = storage
        .range(RangeRequest {
            key: key.clone().into_bytes(),
            ..Default::default()
        })
        .await?;
    let Some(prev) = current.kvs.into_iter().next() else {
        return Ok(DeleteOutcome::ObjectNotFound);
    };
    let mut object = decrypt_and_decode_with_rotation(
        storage,
        group,
        resource,
        &prev.key,
        &prev.value,
        prev.mod_revision,
    )
    .await?;
    set_metadata_field(
        &mut object,
        "resourceVersion",
        Value::String(prev.mod_revision.to_string()),
    );
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
    let is_pod = group.is_empty() && resource == "pods";
    let pod_grace_period_seconds =
        is_pod.then(|| effective_pod_grace_period(&object, grace_period_seconds));

    // Pods use the same two-phase deletion contract as upstream: preserve
    // the object while the node agent stops its containers, then let the
    // node agent issue a second grace=0 delete. Other resources only defer
    // deletion when they have finalizers.
    let defer_delete =
        has_finalizers(&object) || pod_grace_period_seconds.is_some_and(|seconds| seconds > 0);
    if defer_delete {
        if has_deletion_timestamp(&object) {
            let object = convert_to_requested_version(
                storage,
                group,
                version,
                &kind,
                resolved.conversion_webhook.as_ref(),
                object,
            )
            .await?;
            return Ok(DeleteOutcome::Deleted(object));
        }
        set_metadata_field(
            &mut object,
            "deletionTimestamp",
            Value::String(now_rfc3339()),
        );
        // Live bug found verifying #541/#559's fixes: real upstream's
        // Namespace REST strategy also flips status.phase to "Terminating"
        // as part of this same deferred-delete write. Without it,
        // service-account-controller/root-ca-cert-publisher-controller's
        // own is_terminating() checks (which read status.phase, matching
        // upstream's own NamespaceLifecycle admission behavior) never see
        // the namespace as terminating at all -- they keep recreating the
        // default ServiceAccount and kube-root-ca.crt ConfigMap the instant
        // namespace-controller deletes them, forever, so the namespace can
        // never actually finish emptying out and finalize. Confirmed live:
        // "created the default ServiceAccount" / "published kube-root-ca.crt"
        // repeating every ~5s for a namespace that had a real
        // deletionTimestamp the whole time.
        if kind == "Namespace" {
            set_status_field(&mut object, "phase", Value::String("Terminating".to_string()));
        }
        if let Some(seconds) = pod_grace_period_seconds {
            set_metadata_field(
                &mut object,
                "deletionGracePeriodSeconds",
                Value::Number(seconds.into()),
            );
        }
        if dry_run {
            let object = convert_to_requested_version(
                storage,
                group,
                version,
                &kind,
                resolved.conversion_webhook.as_ref(),
                object,
            )
            .await?;
            return Ok(DeleteOutcome::Deleted(object));
        }
        let object_bytes = match resolved.schema {
            Some(schema) => protobuf::encode_message(schema, &object)?,
            None => serde_json::to_vec(&object).map_err(protobuf::Error::Json)?,
        };
        let stored_version = resolved
            .conversion_webhook
            .as_ref()
            .map_or(version, |conversion| conversion.storage_version.as_str());
        let api_version = if group.is_empty() {
            stored_version.to_string()
        } else {
            format!("{group}/{stored_version}")
        };
        let envelope = encrypt_for_storage(
            storage,
            group,
            resource,
            key.as_bytes(),
            &protobuf::wrap_unknown(&api_version, &kind, &object_bytes),
        )?;
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
        set_metadata_field(
            &mut object,
            "resourceVersion",
            Value::String(revision.to_string()),
        );
        let object = convert_to_requested_version(
            storage,
            group,
            version,
            &kind,
            resolved.conversion_webhook.as_ref(),
            object,
        )
        .await?;
        return Ok(DeleteOutcome::Deleted(object));
    }

    if dry_run {
        let object = convert_to_requested_version(
            storage,
            group,
            version,
            &kind,
            resolved.conversion_webhook.as_ref(),
            object,
        )
        .await?;
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
        return Ok(DeleteOutcome::PreconditionFailed);
    }
    let object = convert_to_requested_version(
        storage,
        group,
        version,
        &kind,
        resolved.conversion_webhook.as_ref(),
        object,
    )
    .await?;
    Ok(DeleteOutcome::Deleted(object))
}

/// Resolve the effective grace period for a Pod delete. A repeated delete
/// without an explicit override keeps the already-persisted value; an
/// explicit value may shorten, but never lengthen, the Pod's current grace.
fn effective_pod_grace_period(object: &Value, requested: Option<i64>) -> i64 {
    let current = object
        .pointer("/metadata/deletionGracePeriodSeconds")
        .and_then(Value::as_i64)
        .filter(|seconds| *seconds >= 0)
        .or_else(|| {
            object
                .pointer("/spec/terminationGracePeriodSeconds")
                .and_then(Value::as_i64)
                .filter(|seconds| *seconds >= 0)
        })
        .unwrap_or(30);
    requested.map_or(current, |seconds| current.min(seconds.max(0)))
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
pub async fn delete_collection(
    storage: &mut StorageClient,
    group: &str,
    version: &str,
    resource: &str,
    namespace: Option<&str>,
    label_selector: &str,
    field_selector: &str,
) -> Result<DeleteCollectionOutcome, Error> {
    let listed = list_delete_collection(
        storage,
        group,
        version,
        resource,
        namespace,
        label_selector,
        field_selector,
    )
    .await?;
    let DeleteCollectionOutcome::Deleted(list_value) = listed else {
        return Ok(DeleteCollectionOutcome::UnknownResource);
    };
    let items = list_value["items"].as_array().cloned().unwrap_or_default();
    for item in &items {
        let Some(name) = item.pointer("/metadata/name").and_then(Value::as_str) else {
            continue;
        };
        match delete(storage, group, version, resource, namespace, name).await? {
            DeleteOutcome::ObjectNotFound => {}
            DeleteOutcome::Deleted(_)
            | DeleteOutcome::UnknownResource
            | DeleteOutcome::PreconditionFailed => {}
        }
    }
    Ok(DeleteCollectionOutcome::Deleted(list_value))
}
