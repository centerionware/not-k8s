    if metadata.audit_failures.is_empty() {
        return BTreeMap::new();
    }
    let value = serde_json::to_string(&metadata.audit_failures).unwrap_or_else(|_| "[]".to_string());
