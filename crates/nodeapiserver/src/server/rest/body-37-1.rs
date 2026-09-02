    let existing_list = ephemeral_container_list(existing)?;
    let candidate_list = ephemeral_container_list(candidate)?;
    let mut violations = Vec::new();

    if candidate_list.len() < existing_list.len() {
        violations.push("spec.ephemeralContainers: existing ephemeral containers may not be removed".to_string());
    }
    for (index, old) in existing_list.iter().enumerate() {
        let old_name = old.get("name").and_then(Value::as_str);
        let replacement = old_name.and_then(|name| {
            candidate_list
                .iter()
                .find(|container| container.get("name").and_then(Value::as_str) == Some(name))
        });
        match replacement {
            None => violations.push(format!("spec.ephemeralContainers[{index}]: existing ephemeral containers may not be removed")),
            Some(new) if new != old => {
                violations.push(format!("spec.ephemeralContainers[{index}]: existing ephemeral containers may not be modified"));
            }
            Some(_) => {}
        }
    }

    let ordinary_names: std::collections::BTreeSet<&str> = existing
        .pointer("/spec/containers")
        .into_iter()
        .chain(existing.pointer("/spec/initContainers"))
        .filter_map(Value::as_array)
        .flat_map(|containers| containers.iter())
        .filter_map(|container| container.get("name").and_then(Value::as_str))
        .collect();
    let mut names = std::collections::BTreeSet::new();
    for (index, container) in candidate_list.iter().enumerate() {
        let Some(container) = container.as_object() else {
            violations.push(format!("spec.ephemeralContainers[{index}]: must be an object"));
            continue;
        };
        let Some(name) = container.get("name").and_then(Value::as_str).filter(|name| !name.is_empty()) else {
            violations.push(format!("spec.ephemeralContainers[{index}].name: Required value"));
            continue;
        };
        for detail in crate::scheme::name_format::is_dns1123_label(name) {
            violations.push(format!("spec.ephemeralContainers[{index}].name: {detail}"));
        }
        if !names.insert(name) {
            violations.push(format!("spec.ephemeralContainers[{index}].name: must be unique"));
        }
        if ordinary_names.contains(name) {
            violations.push(format!("spec.ephemeralContainers[{index}].name: must not duplicate a regular or init container"));
        }
        if let Some(target) = container.get("targetContainerName").and_then(Value::as_str).filter(|target| !target.is_empty()) {
            if !ordinary_names.contains(target) {
                violations.push(format!("spec.ephemeralContainers[{index}].targetContainerName: must name an existing regular or init container"));
            }
        }
        for field in ["ports", "resources", "lifecycle", "livenessProbe", "readinessProbe", "startupProbe"] {
            if container.get(field).is_some_and(|value| !value.is_null()) {
                violations.push(format!("spec.ephemeralContainers[{index}].{field}: field is not allowed for an ephemeral container"));
            }
        }
    }
    if !violations.is_empty() {
        return Err(violations);
    }

    let mut object = existing.clone();
    let Some(spec) = object.get_mut("spec").and_then(Value::as_object_mut) else {
        return Err(vec!["spec: must be an object".to_string()]);
    };
    if candidate_list.is_empty() {
        spec.remove("ephemeralContainers");
    } else {
        spec.insert("ephemeralContainers".to_string(), Value::Array(candidate_list.to_vec()));
    }
    if candidate_list != existing_list {
        let generation = object.pointer("/metadata/generation").and_then(Value::as_i64).unwrap_or(0);
        set_metadata_field(&mut object, "generation", Value::Number((generation + 1).into()));
    }
