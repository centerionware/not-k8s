//! Swagger JSON -> gnostic OpenAPI v2 protobuf. Field numbers/types come from
//! google/gnostic-models v0.6.9's vendored OpenAPIv2.proto. Gnostic represents
//! maps as Named* pairs and JSON unions as wrapper messages; these adaptations
//! mirror that model, not the unrelated Kubernetes Unknown/object envelope.
//! Only immutable vendored schema documents enter this conversion.

use prost::Message;
use prost_reflect::{
    DescriptorPool, DynamicMessage, FieldDescriptor, Kind, MessageDescriptor, Value as ProtoValue,
};
use serde_json::{Value, json};

const DESCRIPTOR: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/openapi-v2-descriptor.bin"));

pub(super) fn encode(document: &Value) -> Result<Vec<u8>, String> {
    let pool = DescriptorPool::decode(DESCRIPTOR).map_err(|error| error.to_string())?;
    let descriptor = pool
        .get_message_by_name("openapi.v2.Document")
        .ok_or("missing Document descriptor")?;
    Ok(model(document, descriptor)?.encode_to_vec())
}

fn wrapper<'a>(name: &str, value: &'a Value) -> Result<Option<(&'static str, &'a Value)>, String> {
    let unknown = || format!("invalid {name} discriminator: {value}");
    let field = match name {
        "AdditionalPropertiesItem" => {
            if value.is_boolean() {
                "boolean"
            } else {
                "schema"
            }
        }
        "SchemaItem" => {
            if value["type"] == "file" {
                "file_schema"
            } else {
                "schema"
            }
        }
        "ParametersItem" => {
            if value.get("$ref").is_some() {
                "json_reference"
            } else {
                "parameter"
            }
        }
        "ResponseValue" => {
            if value.get("$ref").is_some() {
                "json_reference"
            } else {
                "response"
            }
        }
        "Parameter" => {
            if value["in"] == "body" {
                "body_parameter"
            } else {
                "non_body_parameter"
            }
        }
        "NonBodyParameter" => match value["in"].as_str().ok_or_else(unknown)? {
            "header" => "header_parameter_sub_schema",
            "formData" => "form_data_parameter_sub_schema",
            "query" => "query_parameter_sub_schema",
            "path" => "path_parameter_sub_schema",
            _ => return Err(unknown()),
        },
        "SecurityDefinitionsItem" => match (value["type"].as_str().ok_or_else(unknown)?, value["flow"].as_str()) {
            ("basic", _) => "basic_authentication_security",
            ("apiKey", _) => "api_key_security",
            ("oauth2", Some("implicit")) => "oauth2_implicit_security",
            ("oauth2", Some("password")) => "oauth2_password_security",
            ("oauth2", Some("application")) => "oauth2_application_security",
            ("oauth2", Some("accessCode")) => "oauth2_access_code_security",
            _ => return Err(unknown()),
        },
        _ => return Ok(None),
    };
    Ok(Some((field, value)))
}

fn scalar(value: &Value, kind: Kind) -> Result<ProtoValue, String> {
    let bad = || format!("invalid OpenAPI protobuf value: {value}");
    match kind {
        Kind::Message(descriptor) => Ok(ProtoValue::Message(model(value, descriptor)?)),
        Kind::String => value
            .as_str()
            .map(|v| ProtoValue::String(v.to_owned()))
            .ok_or_else(bad),
        Kind::Bool => value.as_bool().map(ProtoValue::Bool).ok_or_else(bad),
        Kind::Int64 => value.as_i64().map(ProtoValue::I64).ok_or_else(bad),
        Kind::Double => value.as_f64().map(ProtoValue::F64).ok_or_else(bad),
        other => Err(format!("unhandled OpenAPI protobuf field type: {other:?}")),
    }
}

fn set(message: &mut DynamicMessage, field: &FieldDescriptor, value: &Value) -> Result<(), String> {
    let converted = if field.is_list() {
        let values = value
            .as_array()
            .ok_or_else(|| format!("{} must be an array", field.full_name()))?;
        ProtoValue::List(
            values
                .iter()
                .map(|v| scalar(v, field.kind()))
                .collect::<Result<_, _>>()?,
        )
    } else {
        scalar(value, field.kind())?
    };
    message
        .try_set_field(field, converted)
        .map_err(|error| format!("{}: {error}", field.full_name()))
}

fn model(value: &Value, descriptor: MessageDescriptor) -> Result<DynamicMessage, String> {
    let mut message = DynamicMessage::new(descriptor.clone());
    let name = descriptor.name();
    if let Some((field_name, inner)) = wrapper(name, value)? {
        let field = descriptor
            .get_field_by_name(field_name)
            .ok_or_else(|| format!("{name}.{field_name} missing"))?;
        set(&mut message, &field, inner)?;
        return Ok(message);
    }
    if name == "Any" {
        // JSON is a YAML subset, including arrays/objects/booleans. Gnostic's
        // Any.yaml is how x-kubernetes-* extensions survive this wire format.
        let field = descriptor
            .get_field_by_name("yaml")
            .ok_or("Any.yaml missing")?;
        set(&mut message, &field, &Value::String(value.to_string()))?;
        return Ok(message);
    }
    if matches!(name, "TypeItem" | "StringArray" | "ItemsItem") {
        let field = descriptor
            .get_field_by_name(if name == "ItemsItem" {
                "schema"
            } else {
                "value"
            })
            .ok_or_else(|| format!("{name} wrapper field missing"))?;
        let values = if value.is_array() {
            value.clone()
        } else {
            json!([value])
        };
        set(&mut message, &field, &values)?;
        return Ok(message);
    }
    let object = value
        .as_object()
        .ok_or_else(|| format!("{name} must be an object"))?;
    for field in descriptor.fields() {
        let is_pairs = field.is_list()
            && matches!(field.kind(), Kind::Message(ref m) if m.name().starts_with("Named"));
        if is_pairs {
            let pairs: Vec<Value> = object
                .iter()
                .filter(|(key, _)| match field.name() {
                    "vendor_extension" => key.starts_with("x-"),
                    "path" | "response_code" => !key.starts_with("x-"),
                    "additional_properties" => true,
                    _ => false,
                })
                .map(|(key, value)| json!({"name": key, "value": value}))
                .collect();
            set(&mut message, &field, &Value::Array(pairs))?;
        } else {
            let key = if field.name() == "_ref" {
                "$ref"
            } else {
                field.json_name()
            };
            if let Some(value) = object.get(key) {
                set(&mut message, &field, value)?;
            }
        }
    }
    Ok(message)
}

#[cfg(test)]
mod tests {
    use super::*;
    // A separately generated, typed decoder checks the actual wire contract.
    #[allow(clippy::all)]
    mod pb {
        include!(concat!(env!("OUT_DIR"), "/openapi.v2.rs"));
    }

    #[test]
    fn invalid_wrapper_discriminators_do_not_encode_empty_messages() {
        for parameter in [json!({"name":"bad"}), json!({"in":"unknown", "name":"bad"})] {
            let input = json!({"swagger":"2.0", "parameters":{"bad":parameter}});
            assert!(encode(&input).unwrap_err().contains("NonBodyParameter"));
        }
        for security in [json!({}), json!({"type":"unknown"}),
            json!({"type":"oauth2"}), json!({"type":"oauth2", "flow":"unknown"})] {
            let input = json!({"swagger":"2.0", "securityDefinitions":{"bad":security}});
            assert!(encode(&input).unwrap_err().contains("SecurityDefinitionsItem"));
        }
        assert!(wrapper("Document", &json!({})).unwrap().is_none());
    }

    #[test]
    fn swagger_fields_unions_and_extensions_round_trip_through_gnostic_wire() {
        let input = json!({"swagger":"2.0", "info":{"title":"test", "version":"1"},
            "paths":{"/api/v1/pods":{"post":{"parameters":[{"in":"body", "name":"body",
                "schema":{"$ref":"#/definitions/Pod"}}], "responses":{"200":{"description":"ok"}}}}},
            "definitions":{"Pod":{"type":"object", "properties":{"names":{"type":"array", "items":{"type":"string"}}},
                "additionalProperties":false, "x-kubernetes-group-version-kind":[{"group":"", "version":"v1", "kind":"Pod"}]}}});
        let decoded = pb::Document::decode(encode(&input).unwrap().as_slice()).unwrap();
        assert_eq!(decoded.swagger, "2.0");
        let definitions = decoded.definitions.unwrap();
        let pod = definitions.additional_properties[0].value.as_ref().unwrap();
        assert_eq!(pod.r#type.as_ref().unwrap().value, ["object"]);
        assert_eq!(
            pod.vendor_extension[0].name,
            "x-kubernetes-group-version-kind"
        );
        assert_eq!(
            serde_json::from_str::<Value>(&pod.vendor_extension[0].value.as_ref().unwrap().yaml)
                .unwrap(),
            input["definitions"]["Pod"]["x-kubernetes-group-version-kind"]
        );
        assert_eq!(decoded.paths.unwrap().path[0].name, "/api/v1/pods");
    }

    #[test]
    fn entire_published_schema_is_decodable() {
        let document = super::super::openapi::v2();
        let decoded = pb::Document::decode(encode(&document).unwrap().as_slice()).unwrap();
        assert_eq!(
            decoded.definitions.unwrap().additional_properties.len(),
            document["definitions"].as_object().unwrap().len()
        );
        assert_eq!(
            decoded.paths.unwrap().path.len(),
            document["paths"].as_object().unwrap().len()
        );
    }
}
