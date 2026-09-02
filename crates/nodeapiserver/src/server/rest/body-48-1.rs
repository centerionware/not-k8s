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
