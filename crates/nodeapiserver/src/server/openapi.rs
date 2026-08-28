//! `/openapi/v2` and `/openapi/v3` — the OpenAPI documents used by kubectl
//! and other Kubernetes clients. The v3 documents are served verbatim from
//! the vendored upstream specs. The v2 document is built once from those
//! same specs so both endpoints describe the same API surface.
//!
//! The v3 documents served here are the files `Group A` already vendors
//! verbatim (`codegen::openapi_v3_docs`, built from
//! `vendor/openapi-spec/v3/*.json` by `build/openapi_serve.rs`). This module
//! also derives the v2 document from that same source, while the v3 root
//! discovery index is the shape real `kubectl`/client-go expect at
//! `/openapi/v3` itself (real shape confirmed against upstream's own
//! `staging/src/k8s.io/apiserver/pkg/handler3/handler.go`: a `paths` map
//! from each servable path to a `{serverRelativeURL}` object carrying a
//! `?hash=` cache-busting query parameter) and the routing that serves an
//! individual document's raw bytes back for a request under
//! `/openapi/v3/<path>`.
//!
//! The `?hash=` value is this build's own content hash
//! (`build/openapi_serve.rs`'s doc comment explains why it doesn't need to
//! match whatever algorithm real upstream's apiserver happens to use
//! internally) — a client only ever compares it against a value this same
//! server previously handed back, to decide whether to re-fetch.

use crate::codegen;
use serde_json::{json, Value};
use std::sync::OnceLock;

const OPERATION_METHODS: [&str; 8] = ["get", "put", "post", "delete", "options", "head", "patch", "trace"];

/// `/openapi/v3` — the root discovery index: every servable path, each
/// with a `serverRelativeURL` a client follows to fetch that document.
pub fn root() -> Value {
    let mut paths = serde_json::Map::new();
    for doc in codegen::openapi_v3_docs::OPENAPI_V3_DOCS {
        paths.insert(doc.path.to_string(), json!({"serverRelativeURL": format!("/openapi/v3/{}?hash={}", doc.path, doc.hash)}));
    }
    json!({"paths": paths})
}

/// `/openapi/v2` — a Swagger 2.0 document assembled from the vendored
/// OpenAPI v3 documents. Kubernetes' v3 specs use the same JSON Schema
/// dialect for definitions, so the conversion is deliberately mechanical:
/// component references become definition references, request bodies become
/// Swagger body parameters, and response media types contribute their
/// schema to the response. Keeping this derived from the v3 table avoids a
/// second hand-maintained description of every Kubernetes resource.
pub fn v2() -> Value {
    static DOCUMENT: OnceLock<Value> = OnceLock::new();
    DOCUMENT.get_or_init(build_v2).clone()
}

fn build_v2() -> Value {
    let mut definitions = serde_json::Map::new();
    let mut paths = serde_json::Map::new();

    for document in codegen::openapi_v3_docs::OPENAPI_V3_DOCS {
        let source: Value = serde_json::from_slice(document.content)
            .unwrap_or_else(|error| panic!("vendored OpenAPI v3 document {} is invalid JSON: {error}", document.path));
        let Some(source_object) = source.as_object() else { continue };

        if let Some(schemas) = source_object
            .get("components")
            .and_then(Value::as_object)
            .and_then(|components| components.get("schemas"))
            .and_then(Value::as_object)
        {
            for (name, schema) in schemas {
                // Shared Kubernetes metadata schemas occur in many group
                // documents. They are identical; retain the first copy so
                // the result is deterministic.
                definitions.entry(name.clone()).or_insert_with(|| convert_schema(schema));
            }
        }

        if let Some(source_paths) = source_object.get("paths").and_then(Value::as_object) {
            for (path, path_item) in source_paths {
                paths.insert(path.clone(), convert_path_item(path_item));
            }
        }
    }

    json!({
        "swagger": "2.0",
        "info": {"title": "Kubernetes", "version": "unversioned"},
        "schemes": ["https"],
        "consumes": ["application/json", "application/yaml", "application/vnd.kubernetes.protobuf"],
        "produces": ["application/json", "application/yaml", "application/vnd.kubernetes.protobuf"],
        "paths": paths,
        "definitions": definitions,
    })
}

fn convert_path_item(value: &Value) -> Value {
    let Some(object) = value.as_object() else { return convert_schema(value) };
    let mut converted = serde_json::Map::new();
    if let Some(parameters) = object.get("parameters") {
        converted.insert("parameters".to_string(), convert_parameters(parameters));
    }
    for method in OPERATION_METHODS {
        if let Some(operation) = object.get(method) {
            converted.insert(method.to_string(), convert_operation(operation));
        }
    }
    Value::Object(converted)
}

fn convert_operation(value: &Value) -> Value {
    let Some(object) = value.as_object() else { return convert_schema(value) };
    let mut converted = serde_json::Map::new();
    for (key, value) in object {
        match key.as_str() {
            "parameters" => {
                let parameters = converted
                    .entry(key.clone())
                    .or_insert_with(|| Value::Array(Vec::new()));
                let incoming = convert_parameters(value);
                if let (Some(existing), Some(incoming)) = (parameters.as_array_mut(), incoming.as_array()) {
                    existing.extend(incoming.iter().cloned());
                }
            }
            "requestBody" => {
                if let Some(parameter) = convert_request_body(value) {
                    let parameters = converted
                        .entry("parameters".to_string())
                        .or_insert_with(|| Value::Array(Vec::new()));
                    if let Some(parameters) = parameters.as_array_mut() {
                        parameters.push(parameter);
                    }
                }
            }
            "responses" => {
                converted.insert(key.clone(), convert_responses(value));
            }
            // These are OpenAPI 3-only operation fields. The remaining
            // operation metadata has the same representation in v2.
            "callbacks" | "servers" => {}
            _ => {
                converted.insert(key.clone(), convert_schema(value));
            }
        }
    }
    Value::Object(converted)
}

fn convert_parameters(value: &Value) -> Value {
    Value::Array(
        value
            .as_array()
            .map(|parameters| parameters.iter().map(convert_parameter).collect())
            .unwrap_or_default(),
    )
}

fn convert_parameter(value: &Value) -> Value {
    let Some(object) = value.as_object() else { return convert_schema(value) };
    if object.contains_key("$ref") {
        return convert_schema(value);
    }
    let mut converted = serde_json::Map::new();
    for (key, value) in object {
        if key == "schema" {
            if let Some(schema) = value.as_object() {
                for (schema_key, schema_value) in schema {
                    if matches!(schema_key.as_str(), "type" | "format" | "items" | "collectionFormat" | "default" | "enum" | "maximum" | "minimum" | "maxLength" | "minLength" | "maxItems" | "minItems" | "pattern") {
                        converted.insert(schema_key.clone(), convert_schema(schema_value));
                    }
                }
            }
        } else if !matches!(key.as_str(), "style" | "explode" | "allowReserved") {
            converted.insert(key.clone(), convert_schema(value));
        }
    }
    Value::Object(converted)
}

fn convert_request_body(value: &Value) -> Option<Value> {
    let object = value.as_object()?;
    let schema = object
        .get("content")
        .and_then(Value::as_object)
        .and_then(select_media_schema)?;
    Some(json!({
        "in": "body",
        "name": "body",
        "required": object.get("required").and_then(Value::as_bool).unwrap_or(false),
        "schema": convert_schema(schema),
    }))
}

fn select_media_schema(content: &serde_json::Map<String, Value>) -> Option<&Value> {
    content
        .get("application/json")
        .or_else(|| content.get("*/*"))
        .or_else(|| content.values().next())
        .and_then(Value::as_object)
        .and_then(|media| media.get("schema"))
}

fn convert_responses(value: &Value) -> Value {
    let Some(responses) = value.as_object() else { return convert_schema(value) };
    let mut converted = serde_json::Map::new();
    for (status, response) in responses {
        let Some(response_object) = response.as_object() else {
            converted.insert(status.clone(), convert_schema(response));
            continue;
        };
        let mut output = serde_json::Map::new();
        output.insert(
            "description".to_string(),
            response_object
                .get("description")
                .cloned()
                .unwrap_or_else(|| Value::String("Response".to_string())),
        );
        if let Some(content) = response_object.get("content").and_then(Value::as_object) {
            if let Some(schema) = select_media_schema(content) {
                output.insert("schema".to_string(), convert_schema(schema));
            }
        }
        if let Some(headers) = response_object.get("headers").and_then(Value::as_object) {
            let converted_headers = headers
                .iter()
                .map(|(name, header)| (name.clone(), convert_parameter(header)))
                .collect();
            output.insert("headers".to_string(), Value::Object(converted_headers));
        }
        converted.insert(status.clone(), Value::Object(output));
    }
    Value::Object(converted)
}

/// Recursively copies a JSON Schema/OpenAPI value while translating the
/// only reference namespace change between these documents. OpenAPI v2
/// accepts the Kubernetes vendor extensions and JSON Schema constructs used
/// by the vendored definitions; OpenAPI 3-only nullable/union keywords are
/// retained as vendor extensions where v2 has no equivalent.
fn convert_schema(value: &Value) -> Value {
    match value {
        Value::Object(object) => {
            let mut converted = serde_json::Map::new();
            for (key, value) in object {
                if key == "$ref" {
                    if let Some(reference) = value.as_str() {
                        converted.insert(
                            key.clone(),
                            Value::String(reference.replace("#/components/schemas/", "#/definitions/")),
                        );
                        continue;
                    }
                }
                if key == "nullable" {
                    converted.insert(format!("x-kubernetes-{key}"), convert_schema(value));
                } else if matches!(key.as_str(), "oneOf" | "anyOf" | "not") {
                    converted.insert(format!("x-kubernetes-{key}"), convert_schema(value));
                } else {
                    converted.insert(key.clone(), convert_schema(value));
                }
            }
            Value::Object(converted)
        }
        Value::Array(values) => Value::Array(values.iter().map(convert_schema).collect()),
        _ => value.clone(),
    }
}

/// `/openapi/v3/<path>` — the raw, verbatim vendored document for that
/// path (any `?hash=` query parameter a client sends is accepted but not
/// interpreted; this build always serves its current, single vendored
/// copy of each document rather than a historical version by hash). `None`
/// if this build vendors no such path — a real `404`, not an empty body.
pub fn doc(path: &str) -> Option<&'static [u8]> {
    codegen::openapi_v3_doc_index().get(path).map(|d| d.content)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_lists_a_real_vendored_group_version_with_its_serve_url() {
        let r = root();
        let paths = r["paths"].as_object().unwrap();
        let entry = paths.get("apis/apps/v1").expect("apis/apps/v1 should be a servable OpenAPI v3 path");
        let url = entry["serverRelativeURL"].as_str().unwrap();
        assert!(url.starts_with("/openapi/v3/apis/apps/v1?hash="), "got {url:?}");
    }

    #[test]
    fn root_includes_the_core_v1_groupless_path() {
        let r = root();
        let paths = r["paths"].as_object().unwrap();
        assert!(paths.contains_key("api/v1"), "the groupless core group's own v1 doc should be listed");
    }

    #[test]
    fn v2_is_a_real_swagger_document_derived_from_the_vendored_specs() {
        let document = v2();
        assert_eq!(document["swagger"], "2.0");
        assert_eq!(document["info"]["title"], "Kubernetes");
        assert!(document["paths"].as_object().is_some_and(|paths| paths.contains_key("/apis/apps/v1/")));
        assert!(document["definitions"].as_object().is_some_and(|definitions| definitions.contains_key("io.k8s.api.core.v1.Pod")));
        let pod = &document["definitions"]["io.k8s.api.core.v1.Pod"];
        assert_eq!(pod["properties"]["metadata"]["$ref"], "#/definitions/io.k8s.apimachinery.pkg.apis.meta.v1.ObjectMeta");
    }

    #[test]
    fn v2_converts_openapi3_request_bodies_to_swagger_body_parameters() {
        let document = v2();
        let parameters = &document["paths"]["/api/v1/namespaces"]["post"]["parameters"];
        assert!(parameters.as_array().is_some_and(|parameters| parameters.iter().any(|parameter| parameter["in"] == "body" && parameter["schema"]["$ref"] == "#/definitions/io.k8s.api.core.v1.Namespace")));
    }

    #[test]
    fn doc_serves_real_vendored_json_bytes() {
        let bytes = doc("apis/apps/v1").expect("apis/apps/v1 should be servable");
        let parsed: Value = serde_json::from_slice(bytes).expect("served bytes must be valid JSON");
        // A real OpenAPI v3 document has a top-level "openapi" version
        // field and "components" — cheap structural proof this is the
        // genuine vendored spec, not some placeholder.
        assert!(parsed.get("openapi").is_some());
        assert!(parsed.get("components").is_some());
    }

    #[test]
    fn doc_is_none_for_an_unvendored_path() {
        assert!(doc("apis/totally.made.up/v1").is_none());
    }

    #[test]
    fn every_root_entry_resolves_via_doc() {
        // The two functions must agree with each other: everything root()
        // advertises must actually be fetchable through doc(), and vice
        // versa (same underlying table).
        let r = root();
        for path in r["paths"].as_object().unwrap().keys() {
            assert!(doc(path).is_some(), "{path} is listed in root() but doc() returns None for it");
        }
    }
}
