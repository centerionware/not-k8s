    let prefix = keys::list_prefix("apiextensions.k8s.io", "customresourcedefinitions", None).into_bytes();
    let range_end = prefix_range_end(&prefix);
    let resp = storage.range(RangeRequest { key: prefix, range_end, ..Default::default() }).await?;
    let mut objects = Vec::with_capacity(resp.kvs.len());
    for kv in resp.kvs {
        objects.push(decrypt_and_decode_with_rotation(
            storage,
            "apiextensions.k8s.io",
            "customresourcedefinitions",
            &kv.key,
            &kv.value,
            kv.mod_revision,
        ).await?);
    }
