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
