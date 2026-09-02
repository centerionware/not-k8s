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
        storage, group, version, resource, namespace, name, None, false,
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
/// `dryRun=All`. The read and delete are joined by an MVCC compare so a
/// concurrent update cannot make a validated delete remove a newer object.
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
    if resolve_resource(storage, group, version, resource)
        .await?
        .is_none()
    {
        return Ok(DeleteOutcome::UnknownResource);
    }
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
    let mut object = decrypt_and_decode(storage, group, resource, &prev.key, &prev.value)?;
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
    let object = crate::scheme::conversion::to_version(group, version, &kind, object);
    if dry_run {
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
    let items = list_value["items"].as_array().cloned().unwrap_or_default();
    for item in &items {
        let Some(name) = item.pointer("/metadata/name").and_then(Value::as_str) else {
            continue;
        };
        delete(storage, group, version, resource, namespace, name).await?;
    }
    Ok(DeleteCollectionOutcome::Deleted(list_value))
}
