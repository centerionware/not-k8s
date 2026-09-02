    // Group K: same pruning `create`/`update` run, same order (before
    // validation/defaulting) — `candidate` is already owned, so this
    // just reassigns it rather than needing the borrow-juggling
    // `create`/`update` need for their own `&Value` parameter.
    let candidate = match &context.open_api_schema {
        Some(open_api_schema) => apiextensions::schema_pruning::prune(open_api_schema, &candidate),
        None => candidate,
    };

    let mut violations: Vec<String> = match (context.schema, &context.open_api_schema) {
        (Some(schema), _) => {
            let mut v: Vec<String> = validation::validate_required(schema, &candidate).into_iter().map(|m| format!("{}: Required value", m.path)).collect();
            v.extend(validation::validate_types(schema, &candidate).into_iter().map(|t| format!("{}: expected type {}, got {}", t.path, t.expected, t.actual_kind)));
            v
        }
        // Group K: same scope `create`'s own CRD branch runs.
        (None, Some(open_api_schema)) => {
            let mut v: Vec<String> = apiextensions::schema_validation::validate_required(open_api_schema, &candidate).into_iter().map(|m| format!("{}: Required value", m.path)).collect();
            v.extend(apiextensions::schema_validation::validate_types(open_api_schema, &candidate).into_iter().map(|t| format!("{}: expected type {}, got {}", t.path, t.expected, t.actual_kind)));
            v.extend(apiextensions::schema_validation::validate_constraints(open_api_schema, &candidate));
            v
        }
        (None, None) => Vec::new(),
    };
    violations.extend(name_format_violations(group, resource, name).into_iter().map(|e| format!("metadata.name: {e}")));
    if !violations.is_empty() {
        return Ok(UpdateOutcome::Invalid(violations));
    }

    let object = match (context.schema, &context.open_api_schema) {
        (Some(schema), _) => defaulting::apply_defaults(schema, &candidate),
        (None, Some(open_api_schema)) => apiextensions::schema_defaults::apply_defaults(open_api_schema, &candidate),
        (None, None) => candidate,
    };

    // CEL Phase 4: same real rule evaluation `create`/`update` both run.
    if let Some(open_api_schema) = &context.open_api_schema {
        let rule_violations = apiextensions::cel_evaluate::validate_object(open_api_schema, &object, Some(&context.existing_object));
        if !rule_violations.is_empty() {
            return Ok(UpdateOutcome::Invalid(rule_violations.into_iter().map(|v| v.to_string()).collect()));
        }
    }

