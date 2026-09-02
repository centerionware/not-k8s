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

