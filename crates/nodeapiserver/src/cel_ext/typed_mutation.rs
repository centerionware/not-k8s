//! CEL-Rust declarations for Kubernetes MutatingAdmissionPolicy mutations.
//!
//! Kubernetes does not evaluate mutation expressions as untyped JSON.  The
//! `JSONPatch` operation and the request object's `Object` aliases are named
//! CEL structs, and object aliases follow the request schema down through
//! nested fields and list items.  This module builds those declarations from
//! the same OpenAPI schema the REST layer uses, then converts the resulting
//! CEL value back to JSON for the existing patch/apply code.

use super::Error;
use cel::common::types::{self, Type};
use cel::objects::Key;
use cel::{Context, Env, Program, StructDef, Value as CelValue};
use serde_json::{Map, Value};
use std::collections::BTreeSet;
use std::sync::Arc;

/// Evaluate a mutation expression with Kubernetes' typed `JSONPatch` and
/// `Object` declarations available.  `schema` is the request object's
/// OpenAPI schema; `None` keeps the old JSON-only behavior useful for a
/// resource whose schema could not be resolved.
pub fn eval_json_with_schema_and_cel_vars(
    expression: &str,
    vars: &[(&'static str, &Value)],
    cel_vars: &[(&'static str, CelValue)],
    schema: Option<&Value>,
) -> Result<Value, Error> {
    let mut env = Env::stdlib();
    env.add_struct(json_patch_definition());
    if let Some(schema) = schema {
        add_object_definition(&mut env, "Object", schema, &mut BTreeSet::new());
    }

    let mut context = Context::with_env(Arc::new(env));
    super::register_kubernetes_extensions(&mut context);
    for (name, value) in vars.iter().copied() {
        context
            .add_variable(name, value.clone())
            .map_err(|_| Error::Bind { name })?;
    }
    for (name, value) in cel_vars.iter().cloned() {
        context
            .add_variable(name, value)
            .map_err(|_| Error::Bind { name })?;
    }

    // CEL-Rust checks custom struct fields by exact runtime type. That makes
    // a literal JSONPatch `value` unable to accept both scalars and typed
    // Object structs, although Kubernetes intentionally defines it as dyn.
    // Preserve the typed syntax while evaluating JSONPatch literals as CEL
    // maps; the resulting map is the same JSON shape and typed Object values
    // still pass through the recursive serializer below.
    let expression = rewrite_json_patch_literals(expression);
    let result = Program::compile(&expression)?.execute(&context)?;
    cel_value_to_json(&result)
}

fn rewrite_json_patch_literals(expression: &str) -> String {
    let chars: Vec<char> = expression.chars().collect();
    let mut rewritten = String::with_capacity(expression.len());
    let mut index = 0;
    let mut in_string = false;
    let mut escaped = false;
    while index < chars.len() {
        if in_string {
            rewritten.push(chars[index]);
            if escaped {
                escaped = false;
            } else if chars[index] == '\\' {
                escaped = true;
            } else if chars[index] == '"' {
                in_string = false;
            }
            index += 1;
            continue;
        }
        if chars[index] == '"' {
            in_string = true;
            rewritten.push(chars[index]);
            index += 1;
            continue;
        }
        if token_at(&chars, index, "JSONPatch") {
            let mut open = index + "JSONPatch".chars().count();
            while chars
                .get(open)
                .is_some_and(|character| character.is_whitespace())
            {
                open += 1;
            }
            if chars.get(open) == Some(&'{') {
                rewritten.push('{');
                let (body, close) = rewrite_json_patch_body(&chars, open + 1);
                rewritten.push_str(&body);
                rewritten.push('}');
                index = close + 1;
                continue;
            }
        }
        rewritten.push(chars[index]);
        index += 1;
    }
    rewritten
}

fn token_at(chars: &[char], index: usize, token: &str) -> bool {
    let token_chars: Vec<char> = token.chars().collect();
    chars.get(index..index + token_chars.len()) == Some(token_chars.as_slice())
        && (index == 0 || !is_identifier_char(chars[index - 1]))
        && (index + token_chars.len() == chars.len()
            || !is_identifier_char(chars[index + token_chars.len()]))
}

fn rewrite_json_patch_body(chars: &[char], mut index: usize) -> (String, usize) {
    let mut body = String::new();
    let mut braces = 0;
    let mut brackets = 0;
    let mut parentheses = 0;
    let mut in_string = false;
    let mut escaped = false;
    let mut expect_field = true;

    while index < chars.len() {
        let character = chars[index];
        if in_string {
            body.push(character);
            if escaped {
                escaped = false;
            } else if character == '\\' {
                escaped = true;
            } else if character == '"' {
                in_string = false;
            }
            index += 1;
            continue;
        }
        if character == '"' {
            in_string = true;
            body.push(character);
            index += 1;
            continue;
        }

        if braces == 0 && brackets == 0 && parentheses == 0 && character == '}' {
            return (body, index);
        }
        if braces == 0 && brackets == 0 && parentheses == 0 && character == ',' {
            expect_field = true;
            body.push(character);
            index += 1;
            continue;
        }
        if expect_field
            && braces == 0
            && brackets == 0
            && parentheses == 0
            && is_identifier_start(character)
        {
            let start = index;
            index += 1;
            while index < chars.len() && is_identifier_char(chars[index]) {
                index += 1;
            }
            let mut colon = index;
            while chars.get(colon).is_some_and(|next| next.is_whitespace()) {
                colon += 1;
            }
            if chars.get(colon) == Some(&':') {
                body.push('"');
                body.extend(chars[start..index].iter());
                body.push('"');
                body.extend(chars[index..=colon].iter());
                expect_field = false;
                index = colon + 1;
                continue;
            }
            body.extend(chars[start..index].iter());
            expect_field = false;
            continue;
        }

        match character {
            '{' => braces += 1,
            '}' => braces -= 1,
            '[' => brackets += 1,
            ']' => brackets -= 1,
            '(' => parentheses += 1,
            ')' => parentheses -= 1,
            character if !character.is_whitespace() => expect_field = false,
            _ => {}
        }
        body.push(character);
        index += 1;
    }
    (body, chars.len())
}

fn is_identifier_start(character: char) -> bool {
    character == '_' || character.is_ascii_alphabetic()
}

fn is_identifier_char(character: char) -> bool {
    is_identifier_start(character) || character.is_ascii_digit()
}

/// Convert a CEL result to JSON, including typed CEL structs.  CEL-Rust's
/// built-in `Value::json()` deliberately does not serialize `Struct` values,
/// while Kubernetes mutation expressions return `JSONPatch` and `Object`
/// structs (often nested inside lists or maps).
fn cel_value_to_json(value: &CelValue) -> Result<Value, Error> {
    match value {
        CelValue::List(values) => values
            .iter()
            .map(cel_value_to_json)
            .collect::<Result<Vec<_>, _>>()
            .map(Value::Array),
        CelValue::Map(values) => values
            .map
            .iter()
            .map(|(key, value)| Ok((json_key(key), cel_value_to_json(value)?)))
            .collect::<Result<Map<_, _>, Error>>()
            .map(Value::Object),
        CelValue::Struct(value) => value
            .field_values()
            .into_iter()
            .map(|(name, field)| {
                let field = CelValue::try_from(field.as_ref())
                    .map_err(|error| Error::Serialize(error.to_string()))?;
                Ok((name, cel_value_to_json(&field)?))
            })
            .collect::<Result<Map<_, _>, Error>>()
            .map(Value::Object),
        _ => value
            .json()
            .map_err(|error| Error::Serialize(error.to_string())),
    }
}

fn json_key(key: &Key) -> String {
    match key {
        Key::Int(value) => value.to_string(),
        Key::Uint(value) => value.to_string(),
        Key::Bool(value) => value.to_string(),
        Key::String(value) => value.as_str().to_owned(),
    }
}

/// Evaluate a typed mutation under the same bounded worker-thread deadline
/// used by the ordinary admission CEL helpers.
pub fn eval_json_with_schema_and_cel_vars_and_deadline(
    expression: &str,
    vars: &[(&'static str, &Value)],
    cel_vars: &[(&'static str, CelValue)],
    schema: Option<&Value>,
    deadline: std::time::Duration,
) -> Result<Value, Error> {
    let expression = expression.to_string();
    let owned_vars: Vec<(&'static str, Value)> = vars
        .iter()
        .map(|(name, value)| (*name, (*value).clone()))
        .collect();
    let owned_cel_vars: Vec<(&'static str, CelValue)> = cel_vars
        .iter()
        .map(|(name, value)| (*name, value.clone()))
        .collect();
    let schema = schema.cloned();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let borrowed_vars: Vec<(&'static str, &Value)> = owned_vars
            .iter()
            .map(|(name, value)| (*name, value))
            .collect();
        let result = eval_json_with_schema_and_cel_vars(
            &expression,
            &borrowed_vars,
            &owned_cel_vars,
            schema.as_ref(),
        );
        let _ = tx.send(result);
    });
    rx.recv_timeout(deadline)
        .unwrap_or(Err(Error::DeadlineExceeded))
}

fn json_patch_definition() -> StructDef {
    StructDef::new("JSONPatch".to_string())
        .add_field("op".to_string(), types::STRING_TYPE.to_owned())
        .add_field("from".to_string(), types::STRING_TYPE.to_owned())
        .add_field("path".to_string(), types::STRING_TYPE.to_owned())
        .add_field("value".to_string(), types::DYN_TYPE.to_owned())
}

fn add_object_definition(env: &mut Env, name: &str, schema: &Value, seen: &mut BTreeSet<String>) {
    if !seen.insert(name.to_string()) {
        return;
    }

    let mut definition = StructDef::new(name.to_string());
    for (field, field_schema) in schema_properties(schema) {
        let field_name = format!("{name}.{field}");
        let field_type = field_type(env, &field_name, &field_schema, seen);
        definition = definition.add_field(field, field_type);
    }
    env.add_struct(definition);
}

fn field_type(env: &mut Env, name: &str, schema: &Value, seen: &mut BTreeSet<String>) -> Type {
    let properties = schema_properties(schema);
    if !properties.is_empty() {
        add_object_definition(env, name, schema, seen);
        return Type::new_struct(name.to_string());
    }
    if schema.get("type").and_then(Value::as_str) == Some("object") {
        if schema.get("additionalProperties").is_none() {
            add_object_definition(env, name, schema, seen);
            return Type::new_struct(name.to_string());
        }
        return types::MAP_TYPE.to_owned();
    }

    match schema.get("type").and_then(Value::as_str) {
        Some("array") => {
            if let Some(item_schema) = schema.get("items") {
                if !schema_properties(item_schema).is_empty()
                    || item_schema.get("type").and_then(Value::as_str) == Some("object")
                {
                    // Kubernetes documents both forms in policy examples;
                    // register the list field and its explicit `.item`
                    // spelling as aliases for the element object type.
                    add_object_definition(env, name, item_schema, seen);
                    add_object_definition(env, &format!("{name}.item"), item_schema, seen);
                }
            }
            types::LIST_TYPE.to_owned()
        }
        Some("string") => types::STRING_TYPE.to_owned(),
        Some("boolean") => types::BOOL_TYPE.to_owned(),
        Some("integer") => types::INT_TYPE.to_owned(),
        Some("number") => types::DOUBLE_TYPE.to_owned(),
        _ => types::DYN_TYPE.to_owned(),
    }
}

/// Merge object properties from `properties` and OpenAPI `allOf` wrappers.
/// The REST schemas use `allOf` for referenced Kubernetes types, while CRD
/// schemas generally use direct properties.
fn schema_properties(schema: &Value) -> Vec<(String, Value)> {
    let mut properties = std::collections::BTreeMap::new();
    if let Some(object) = schema.get("properties").and_then(Value::as_object) {
        properties.extend(
            object
                .iter()
                .map(|(name, schema)| (name.clone(), schema.clone())),
        );
    }
    if let Some(all_of) = schema.get("allOf").and_then(Value::as_array) {
        for item in all_of {
            properties.extend(schema_properties(item));
        }
    }
    properties.into_iter().collect()
}

/// Unit-test-only convenience for a small schema without REST/storage.
#[cfg(test)]
fn test_schema() -> Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "spec": {
                "type": "object",
                "properties": {
                    "replicas": {"type": "integer"},
                    "selector": {
                        "type": "object",
                        "properties": {
                            "matchLabels": {
                                "type": "object",
                                "additionalProperties": {"type": "string"}
                            }
                        }
                    },
                    "containers": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "name": {"type": "string"},
                                "image": {"type": "string"}
                            }
                        }
                    }
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jsonpatch_rewriting_preserves_strings_and_quotes_fields() {
        assert_eq!(
            rewrite_json_patch_literals(r#"[JSONPatch{op: "add", path: "/x", value: "y"}]"#),
            r#"[{"op": "add", "path": "/x", "value": "y"}]"#
        );
        let expression = r#""JSONPatch{op: "add"}""#;
        assert_eq!(rewrite_json_patch_literals(expression), expression);
    }

    #[test]
    fn typed_jsonpatch_structs_serialize_nested_object_values() {
        let schema = test_schema();
        let value = eval_json_with_schema_and_cel_vars(
            r#"[JSONPatch{op: "add", path: "/spec/selector", value: Object.spec.selector{matchLabels: {"environment": "test"}}}]"#,
            &[],
            &[],
            Some(&schema),
        )
        .unwrap();
        assert_eq!(
            value,
            serde_json::json!([{
                "op": "add",
                "path": "/spec/selector",
                "value": {"matchLabels": {"environment": "test"}}
            }])
        );
    }

    #[test]
    fn typed_apply_configuration_serializes_the_object_alias() {
        let schema = test_schema();
        let value = eval_json_with_schema_and_cel_vars(
            r#"Object{spec: Object.spec{replicas: 3}}"#,
            &[],
            &[],
            Some(&schema),
        )
        .unwrap();
        assert_eq!(value, serde_json::json!({"spec": {"replicas": 3}}));
    }

    #[test]
    fn typed_list_item_aliases_are_available_in_both_upstream_spellings() {
        let schema = test_schema();
        for expression in [
            r#"Object{spec: Object.spec{containers: [Object.spec.containers{name: "web", image: "example/web"}]}}"#,
            r#"Object{spec: Object.spec{containers: [Object.spec.containers.item{name: "web", image: "example/web"}]}}"#,
        ] {
            let value =
                eval_json_with_schema_and_cel_vars(expression, &[], &[], Some(&schema)).unwrap();
            assert_eq!(value["spec"]["containers"][0]["name"], "web");
        }
    }

    #[test]
    fn an_unknown_typed_object_field_is_rejected() {
        let schema = test_schema();
        let result = eval_json_with_schema_and_cel_vars(
            r#"Object{spec: Object.spec{notAField: true}}"#,
            &[],
            &[],
            Some(&schema),
        );
        assert!(result.is_err());
    }
}
