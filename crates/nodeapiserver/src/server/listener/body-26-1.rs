    let Some(preconditions) = value.and_then(|value| value.get("preconditions")) else {
        return Ok(None);
    };
    let Some(preconditions) = preconditions.as_object() else {
        return Err("metadata.preconditions must be an object");
    };
    let string_field = |name: &str| -> Result<Option<String>, &'static str> {
        match preconditions.get(name) {
            None | Some(serde_json::Value::Null) => Ok(None),
            Some(value) => value.as_str().map(|value| Some(value.to_string())).ok_or("delete preconditions must be strings"),
        }
    };
