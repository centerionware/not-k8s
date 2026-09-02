
fn expand_openapi_refs(value: &Value, schemas: &Map<String, Value>, active: &mut BTreeSet<String>) -> Value {
    match value {
        Value::Array(values) => Value::Array(values.iter().map(|value| expand_openapi_refs(value, schemas, active)).collect()),
        Value::Object(object) => {
            if let Some(reference) = object.get("$ref").and_then(Value::as_str).and_then(|reference| reference.strip_prefix("#/components/schemas/")) {
                if let Some(target) = schemas.get(reference) {
                    if active.insert(reference.to_string()) {
                        let mut expanded = expand_openapi_refs(target, schemas, active);
                        active.remove(reference);
                        if let Value::Object(expanded_object) = &mut expanded {
                            for (key, value) in object {
                                if key != "$ref" {
                                    expanded_object.insert(key.clone(), expand_openapi_refs(value, schemas, active));
                                }
                            }
                        }
                        return expanded;
                    }
                    // Recursive OpenAPI types cannot be represented by a
                    // finite CEL struct tree. Keep the recursive edge
                    // dynamic while retaining the containing object fields.
                    return json!({"type": "object"});
                }
            }
            Value::Object(object.iter().map(|(key, value)| (key.clone(), expand_openapi_refs(value, schemas, active))).collect())
        }
        _ => value.clone(),
    }
}
