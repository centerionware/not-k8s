    let (api_version, kind, object_bytes) = protobuf::unwrap_unknown(bytes)?;
    let (group, version) = split_api_version(&api_version);
    let mut object = match protobuf::schema_for_gvk(group, version, &kind) {
        Some(schema) => protobuf::decode_message(schema, &object_bytes),
        // Group K: no compiled schema for this Kind at all -- a CRD-
        // defined object, which `server::rest`'s write side always
        // stores as raw JSON in the envelope's `raw` field rather than
        // protobuf-encoding it (there's no compiled schema to encode
        // *with* either). A genuinely unknown, non-CRD Kind decodes to
        // the same `Json` error a malformed CRD body would -- this
        // function has no registry to tell the two apart, and both are
        // real "can't decode this" outcomes either way.
        None => Ok(serde_json::from_slice(&object_bytes).map_err(protobuf::Error::Json)?),
    }?;
    set_type_metadata(&mut object, &kind, &api_version);
