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
