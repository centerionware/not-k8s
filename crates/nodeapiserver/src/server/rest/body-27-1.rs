    let Some(resolved) = resolve_resource(storage, group, version, resource).await? else {
        return Ok(ListOutcome::UnknownResource);
    };
    let kind = resolved.kind.as_str();
    let label_reqs = if label_selector.is_empty() { Vec::new() } else { selector::parse_label_selector(label_selector)? };
    let field_reqs = if field_selector.is_empty() { Vec::new() } else { selector::parse_field_selector(field_selector)? };
    selector::validate_field_selector(group, resource, &field_reqs)?;

    let group_version = if group.is_empty() { version.to_string() } else { format!("{group}/{version}") };
    // Shared by both the cache path and the direct-nodestore path below —
    // the cache registers one entry per whole `(group, version, resource)`
    // (`cacher::registry`'s own doc comment: "every namespace at once, not
    // one cache per namespace"), so a namespaced request still needs this
    // same prefix to scope the cache's own entries down to one namespace,
    // exactly as it already scopes the `Range` request on the fallback path.
    let prefix = keys::list_prefix(group, resource, namespace).into_bytes();

    // Real upstream itself doesn't serve a paginated request from its own
    // watch cache either — a consistent ordered range-scan-with-resume-point
    // is what the underlying store gives for free and an in-memory
    // unordered cache doesn't. A paginated request (real `limit`/`continue`,
    // not the default "everything") always goes straight to nodestore
    // below, same as an unsynced cache would.
    let paginated = limit > 0 || !continue_token.is_empty() || resource_version > 0;
    if let Some(cache) = cache {
        if cache.has_synced() && !paginated {
            let (entries, revision) = cache.list();
            let mut items = Vec::new();
            for (key, entry) in entries.iter().filter(|(key, _)| key.starts_with(&prefix)) {
                // Same real fix `get`'s own doc comment covers: a stored
                // object never carries `resourceVersion` as persisted
                // content, so every item in a `LIST` response needs it
                // stamped from its own live revision, the same way real
                // upstream does.
                let mut object = decrypt_and_decode_with_rotation(storage, group, resource, key, &entry.value, entry.mod_revision).await?;
                set_metadata_field(&mut object, "resourceVersion", Value::String(entry.mod_revision.to_string()));
                object = convert_to_requested_version(storage, group, version, kind, resolved.conversion_webhook.as_ref(), object).await?;
                if selector::object_matches(&object, &label_reqs, &field_reqs) {
                    items.push(object);
                }
            }
            return Ok(ListOutcome::Found(json!({
                "kind": list_kind(kind),
                "apiVersion": group_version,
                "metadata": {"resourceVersion": revision.to_string()},
                "items": items,
            })));
        }
    }

    let range_end = prefix_range_end(&prefix);
    // A `continue` token resumes from the exact key its own page left off
    // at (`encode_continue_token`'s own doc comment covers the "append a
    // single 0x00 byte" idiom that makes this the correct etcd range
    // start), at the same revision the listing began at — every page of
    // one listing sees a consistent snapshot, matching real upstream's
    // own pagination contract.
    let (start_key, at_revision) = if continue_token.is_empty() {
        (prefix, resource_version.max(0))
    } else {
        match decode_continue_token(continue_token) {
            Some((key, revision)) => (key, revision),
            None => return Ok(ListOutcome::InvalidContinueToken),
        }
    };
    let resp = storage.range(RangeRequest { key: start_key, range_end, limit: limit.max(0), revision: at_revision, ..Default::default() }).await?;
    let revision = resp.header.map(|h| h.revision).unwrap_or(at_revision);
    // Real upstream's own documented caveat applies here too: filtering by
    // label/field selector happens *after* the limited range fetch, so a
    // page can legitimately come back with fewer than `limit` items (or
    // even zero) despite there being more matching items on later pages —
    // this isn't a bug, it's the same trade-off a selector combined with
    // `limit` has against a real etcd-backed apiserver.
    let more = resp.more;
    // The successor marker `encode_continue_token`'s own doc comment
    // expects — appended *here*, not inside that function, so its own
    // internal `0x00` push stays purely about the encoding's key/revision
    // separator (see that function's doc comment for why the two
    // 0x00 bytes this produces when they land back to back is
    // deliberate, not a bug).
    let resume_key = resp.kvs.last().map(|kv| {
        let mut k = kv.key.clone();
        k.push(0);
        k
    });
    let mut items = Vec::with_capacity(resp.kvs.len());
    for kv in &resp.kvs {
        let mut object = decrypt_and_decode_with_rotation(storage, group, resource, &kv.key, &kv.value, kv.mod_revision).await?;
        set_metadata_field(&mut object, "resourceVersion", Value::String(kv.mod_revision.to_string()));
        object = convert_to_requested_version(storage, group, version, kind, resolved.conversion_webhook.as_ref(), object).await?;
        if selector::object_matches(&object, &label_reqs, &field_reqs) {
            items.push(object);
        }
    }

    let mut metadata = json!({"resourceVersion": revision.to_string()});
    if more {
        if let Some(resume_key) = resume_key {
            metadata["continue"] = json!(encode_continue_token(&resume_key, revision));
        }
    }

