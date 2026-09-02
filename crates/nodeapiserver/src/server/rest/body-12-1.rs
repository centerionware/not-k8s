    let Some(schema) = schema else {
        return Ok(object);
    };
    let object = apiextensions::schema_pruning::prune(schema, &object);
    let mut violations = apiextensions::schema_validation::validate_required(schema, &object)
        .into_iter()
        .map(|violation| format!("{}: Required value", violation.path))
        .collect::<Vec<_>>();
    violations.extend(
        apiextensions::schema_validation::validate_types(schema, &object)
            .into_iter()
            .map(|violation| format!("{}: expected type {}, got {}", violation.path, violation.expected, violation.actual_kind)),
    );
    violations.extend(apiextensions::schema_validation::validate_constraints(schema, &object));
