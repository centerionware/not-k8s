    let inner = proto_type
        .strip_prefix("map<")
        .and_then(|s| s.strip_suffix('>'))
        .ok_or_else(|| Error::MalformedMapType(proto_type.to_string()))?;
    let (k, v) = inner.split_once(',').ok_or_else(|| Error::MalformedMapType(proto_type.to_string()))?;
    // Leaked once per distinct map type at parse time — a small, bounded
    // set (map field variants are rare in the k8s API), and ProtoField's
    // fields are `&'static str` throughout, so this keeps the synthetic
    // key/value ProtoFields built above the same shape as every real one
    // rather than introducing an owned-string variant just for this case.
    let k: &'static str = Box::leak(k.trim().to_string().into_boxed_str());
    let v: &'static str = Box::leak(v.trim().to_string().into_boxed_str());
