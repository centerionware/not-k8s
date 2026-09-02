    let Some(schema) = open_api_schema.as_ref() else { return Vec::new() };
    let Some(status_schema) = schema.pointer("/properties/status") else { return Vec::new() };
    let Some(status) = object.get("status").cloned() else { return Vec::new() };

    let wrapper_schema = json!({
        "type": "object",
        "properties": {"status": status_schema}
    });
    let candidate = json!({"status": status});
    let pruned = apiextensions::schema_pruning::prune(&wrapper_schema, &candidate);
    let mut violations: Vec<String> = apiextensions::schema_validation::validate_required(&wrapper_schema, &pruned)
        .into_iter()
        .map(|violation| format!("{}: Required value", violation.path))
        .collect();
    violations.extend(
        apiextensions::schema_validation::validate_types(&wrapper_schema, &pruned)
            .into_iter()
            .map(|violation| format!("{}: expected type {}, got {}", violation.path, violation.expected, violation.actual_kind)),
    );
    violations.extend(apiextensions::schema_validation::validate_constraints(&wrapper_schema, &pruned));
    if violations.is_empty() {
        if let Some(status) = pruned.get("status") {
            object["status"] = status.clone();
        }
    }
