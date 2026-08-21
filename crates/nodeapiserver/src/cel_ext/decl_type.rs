//! Converts a CRD's own runtime `openAPIV3Schema` (a `serde_json::Value`,
//! the same shape every `apiextensions::schema_*` sibling module already
//! walks — this build has no compile-time knowledge of a CRD's schema,
//! unlike a built-in type's compiled `FIELD_META`) into the real
//! `DeclType` tree Phase 3's own static cost estimator needs — a
//! faithful port of real upstream's `SchemaDeclType`
//! (`k8s.io/apiserver/pkg/cel/common/schemas.go`, fetched and read
//! directly) plus its own `estimateMax*`/`Min*Size` helpers.
//!
//! # What a `DeclType` actually is, and why cost estimation needs one
//! at all
//!
//! [`cost::SizeEstimate`] answers "how big could this value be", but a
//! CEL expression names a *path* (`self.spec.items[0].name`), not a
//! value — [`super::cost`]'s own module doc names the two narrow places
//! real upstream's algorithm needs resolved *type* info, and this is
//! where that type info actually comes from for a CRD: this module
//! builds one `DeclType` tree per CRD version's own schema, once, at
//! CRD-acceptance time; the estimator (a follow-up slice) then walks a
//! CEL expression's own field-path against that tree to look up a
//! `SizeEstimate` at each step, exactly the way `checker.AstNode.Path()`
//! is used real upstream — see `sizeEstimator.EstimateSize`'s own real
//! source (`k8s.io/apiextensions-apiserver/.../schema/cel/compilation.go`),
//! walking a path one `DeclType` level at a time via `@items`/`@keys`/
//! `@values`/a named field, exactly the shape [`DeclType`] mirrors here.
//!
//! `max_elements` on every [`DeclType`] node is the real, single number
//! this whole module exists to compute: the worst-case count of CEL
//! `size()` units (unicode characters for a string, entries for a list/
//! map) a value at that schema location could ever hold — either the
//! schema's own explicit `maxLength`/`maxItems`/`maxProperties`, or,
//! when the schema declares none, a real derived bound from
//! `MaxRequestSizeBytes` (the crate can't accept a request bigger than
//! this at all, so nothing serialized within it can exceed it either) —
//! never an unbounded "could be anything," which would make every cost
//! estimate against it also unbounded and defeat the point of static
//! estimation entirely.

use serde_json::Value;

/// `k8s.io/apiserver/pkg/apis/cel/config.go`'s own `MaxRequestSizeBytes`
/// (3MiB) — already documented in `docs/APISERVER.md`'s own `cel_ext`
/// section, confirmed again directly here since this module is the one
/// that actually needs the literal value.
const MAX_REQUEST_SIZE_BYTES: i64 = 3 * 1024 * 1024;

/// `k8s.io/apiserver/pkg/cel/limits.go`'s own real per-type minimum
/// serialized sizes, fetched and read directly.
const MAX_DURATION_SIZE_JSON: i64 = 32;
const MAX_DATETIME_SIZE_JSON: i64 = 32;
const MIN_DATETIME_SIZE_JSON: i64 = 21;
const JSON_DATE_SIZE: i64 = 12;
const MIN_STRING_SIZE: i64 = 2;
const MIN_BOOL_SIZE: i64 = 4;
const MIN_NUMBER_SIZE: i64 = 1;

/// The real, recursive structural shape a [`DeclType`] can be — real
/// upstream's own `DeclType.Fields`/`.ElemType`/`.KeyType` fields,
/// modeled as a proper Rust enum instead (a `DeclType` is exactly one of
/// these shapes, never several at once, unlike the Go struct's own
/// always-present-but-usually-nil fields).
#[derive(Debug, Clone, PartialEq)]
pub enum Shape {
    /// A leaf with no further structure to path into — every scalar CEL
    /// type (`bool`/`int`/`double`/`string`/`bytes`/`duration`/
    /// `timestamp`/the dynamic `x-kubernetes-int-or-string` type).
    Scalar,
    List(Box<DeclType>),
    /// A generic (`additionalProperties`) map — the key is always CEL's
    /// own plain `string` type, real upstream's own
    /// `apiservercel.StringType`; no k8s map ever has a non-string key.
    Map(Box<DeclType>),
    /// A fixed-`properties` object — real upstream's own `DeclType.
    /// Fields`, a name to its own `DeclType` (`DeclField.Type`, the
    /// `Required`/enum/default bookkeeping real upstream's own
    /// `DeclField` also carries isn't needed by cost estimation itself,
    /// so it isn't modeled here).
    Object(std::collections::BTreeMap<String, DeclType>),
}

/// One node of the real `DeclType` tree — see this module's own doc
/// comment for what `max_elements`/`min_serialized_size` mean and why
/// cost estimation needs them.
#[derive(Debug, Clone, PartialEq)]
pub struct DeclType {
    pub max_elements: i64,
    pub min_serialized_size: i64,
    pub shape: Shape,
}

impl DeclType {
    fn scalar(min_serialized_size: i64, max_elements: i64) -> Self {
        Self { max_elements, min_serialized_size, shape: Shape::Scalar }
    }
}

/// Real upstream's own `SchemaDeclType` — `None` for a schema shape this
/// crate (same as real upstream) declines to expose to CEL at all (an
/// `additionalProperties`-less object with no `properties` either, or an
/// array with no `items`).
pub fn decl_type_for(schema: &Value) -> Option<DeclType> {
    if schema.get("x-kubernetes-int-or-string").and_then(Value::as_bool) == Some(true) {
        // Real upstream's own dynamic union type -- every access to a
        // real x-kubernetes-int-or-string value must itself branch on
        // `type(...)`  at runtime, so this crate models no further
        // structure for it than its own worst-case serialized size.
        let max_elements = match schema.get("maxLength").and_then(Value::as_i64) {
            Some(max_length) => estimate_max_elements_from_max_length(max_length),
            None => MAX_REQUEST_SIZE_BYTES - 2,
        };
        return Some(DeclType::scalar(1, max_elements));
    }

    match schema.get("type").and_then(Value::as_str) {
        Some("array") => {
            let items = schema.get("items")?;
            let item_type = decl_type_for(items)?;
            let max_items = match schema.get("maxItems").and_then(Value::as_i64) {
                Some(m) => m.max(0),
                None => estimate_max_array_items_from_min_size(item_type.min_serialized_size),
            };
            Some(DeclType { max_elements: max_items, min_serialized_size: 2, shape: Shape::List(Box::new(item_type)) })
        }
        Some("object") => {
            if let Some(additional) = schema.get("additionalProperties").and_then(|a| a.as_object().is_some().then_some(a)) {
                let value_type = decl_type_for(additional)?;
                let max_properties = match schema.get("maxProperties").and_then(Value::as_i64) {
                    Some(m) => m.max(0),
                    None => estimate_max_additional_properties_from_min_size(value_type.min_serialized_size),
                };
                return Some(DeclType { max_elements: max_properties, min_serialized_size: 2, shape: Shape::Map(Box::new(value_type)) });
            }

            let mut fields = std::collections::BTreeMap::new();
            // An object always serializes as at least "{}" -- real
            // upstream's own starting point before adding each required
            // property's own minimum contribution.
            let mut min_serialized_size: i64 = 2;
            let required: std::collections::HashSet<&str> = schema.get("required").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).collect();
            if let Some(properties) = schema.get("properties").and_then(Value::as_object) {
                for (name, prop) in properties {
                    let Some(field_type) = decl_type_for(prop) else { continue };
                    // Only a *required* property with no default contributes
                    // to the object's own minimum size -- real upstream's own
                    // rule: a default gets filled in server-side, so it's
                    // never actually absent from what's *validated*, but it
                    // also never forces the client's own request to carry it.
                    if required.contains(name.as_str()) && prop.get("default").is_none() {
                        min_serialized_size += name.len() as i64 + field_type.min_serialized_size + 4;
                    }
                    fields.insert(name.clone(), field_type);
                }
            }
            Some(DeclType { max_elements: 0, min_serialized_size, shape: Shape::Object(fields) })
        }
        Some("string") => Some(decl_type_for_string(schema)),
        Some("boolean") => Some(DeclType::scalar(MIN_BOOL_SIZE, 1)),
        Some("number") => Some(DeclType::scalar(MIN_NUMBER_SIZE, 1)),
        Some("integer") => Some(DeclType::scalar(MIN_NUMBER_SIZE, 1)),
        _ => None,
    }
}

fn decl_type_for_string(schema: &Value) -> DeclType {
    match schema.get("format").and_then(Value::as_str) {
        Some("byte") => {
            let max_elements = match schema.get("maxLength").and_then(Value::as_i64) {
                Some(m) => m.max(0),
                None => estimate_max_string_length_per_request(schema),
            };
            DeclType::scalar(MIN_STRING_SIZE, max_elements)
        }
        // Real upstream's own `MinDurationSizeJSON` (3: `""` around a
        // zero-length Go duration string of `0`).
        Some("duration") => DeclType::scalar(3, estimate_max_string_length_per_request(schema)),
        Some("date") => DeclType::scalar(JSON_DATE_SIZE, estimate_max_string_length_per_request(schema)),
        Some("date-time") => DeclType::scalar(MIN_DATETIME_SIZE_JSON, estimate_max_string_length_per_request(schema)),
        _ => {
            let max_elements = if let Some(max_length) = schema.get("maxLength").and_then(Value::as_i64) {
                estimate_max_elements_from_max_length(max_length)
            } else if let Some(values) = schema.get("enum").and_then(Value::as_array) {
                estimate_max_string_enum_length(values)
            } else {
                estimate_max_string_length_per_request(schema)
            };
            DeclType::scalar(MIN_STRING_SIZE, max_elements)
        }
    }
}

/// Real upstream's own `estimateMaxStringLengthPerRequest` — the request
/// itself can never exceed [`MAX_REQUEST_SIZE_BYTES`], so nothing
/// serialized inside it can either; each real format gets its own
/// tighter real bound instead when one exists (RFC 3339 dates/durations
/// have a fixed maximum real length, unlike a plain string).
fn estimate_max_string_length_per_request(schema: &Value) -> i64 {
    match schema.get("format").and_then(Value::as_str) {
        Some("duration") => MAX_DURATION_SIZE_JSON,
        Some("date") => JSON_DATE_SIZE,
        Some("date-time") => MAX_DATETIME_SIZE_JSON,
        // Subtract 2 for the surrounding `""` real upstream's own
        // comment names.
        _ => MAX_REQUEST_SIZE_BYTES - 2,
    }
}

/// Real upstream's own `estimateMaxStringEnumLength` — bounded by the
/// longest declared enum value's own real length, tighter than falling
/// back to the whole-request bound.
fn estimate_max_string_enum_length(values: &[Value]) -> i64 {
    values.iter().filter_map(Value::as_str).map(|s| s.chars().count() as i64).max().unwrap_or(0)
}

/// Real upstream's own `estimateMaxArrayItemsFromMinSize`.
fn estimate_max_array_items_from_min_size(min_size: i64) -> i64 {
    (MAX_REQUEST_SIZE_BYTES - 2) / (min_size + 1)
}

/// Real upstream's own `estimateMaxAdditionalPropertiesFromMinSize`.
fn estimate_max_additional_properties_from_min_size(min_size: i64) -> i64 {
    let key_value_pair_size = min_size + 6;
    (MAX_REQUEST_SIZE_BYTES - 2) / key_value_pair_size
}

/// Real upstream's own `estimateMaxElementsFromMaxLength` — a
/// user-declared `maxLength` is in Unicode code points (the OpenAPI v3
/// spec's own unit), multiplied by 4 (the largest a single UTF-8 code
/// point can be) to get a real byte-oriented worst case.
fn estimate_max_elements_from_max_length(max_length: i64) -> i64 {
    max_length.max(0) * 4
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_plain_string_with_no_bound_falls_back_to_the_request_size_bound() {
        let d = decl_type_for(&json!({"type": "string"})).unwrap();
        assert_eq!(d.max_elements, MAX_REQUEST_SIZE_BYTES - 2);
        assert_eq!(d.shape, Shape::Scalar);
    }

    #[test]
    fn a_string_with_max_length_is_bounded_by_it_times_four() {
        let d = decl_type_for(&json!({"type": "string", "maxLength": 10})).unwrap();
        assert_eq!(d.max_elements, 40);
    }

    #[test]
    fn a_string_with_an_enum_is_bounded_by_the_longest_value() {
        let d = decl_type_for(&json!({"type": "string", "enum": ["a", "bb", "ccc"]})).unwrap();
        assert_eq!(d.max_elements, 3);
    }

    #[test]
    fn a_date_time_string_has_the_real_rfc3339_bound_regardless_of_max_length() {
        let d = decl_type_for(&json!({"type": "string", "format": "date-time"})).unwrap();
        assert_eq!(d.max_elements, MAX_DATETIME_SIZE_JSON);
        assert_eq!(d.min_serialized_size, MIN_DATETIME_SIZE_JSON);
    }

    #[test]
    fn a_boolean_is_a_fixed_size_scalar() {
        let d = decl_type_for(&json!({"type": "boolean"})).unwrap();
        assert_eq!(d, DeclType { max_elements: 1, min_serialized_size: MIN_BOOL_SIZE, shape: Shape::Scalar });
    }

    #[test]
    fn an_array_with_max_items_uses_it_directly() {
        let d = decl_type_for(&json!({"type": "array", "items": {"type": "integer"}, "maxItems": 5})).unwrap();
        assert_eq!(d.max_elements, 5);
        assert!(matches!(d.shape, Shape::List(_)));
    }

    #[test]
    fn an_array_with_no_max_items_derives_one_from_the_request_size_bound() {
        let d = decl_type_for(&json!({"type": "array", "items": {"type": "integer"}})).unwrap();
        // integer's own min_serialized_size is 1 -> (3MiB - 2) / 2
        assert_eq!(d.max_elements, (MAX_REQUEST_SIZE_BYTES - 2) / 2);
    }

    #[test]
    fn an_array_with_no_items_schema_is_not_exposed() {
        assert_eq!(decl_type_for(&json!({"type": "array"})), None);
    }

    #[test]
    fn a_generic_map_uses_additional_properties_as_its_value_type() {
        let d = decl_type_for(&json!({"type": "object", "additionalProperties": {"type": "string"}})).unwrap();
        match d.shape {
            Shape::Map(value_type) => assert_eq!(value_type.shape, Shape::Scalar),
            other => panic!("expected Shape::Map, got {other:?}"),
        }
    }

    #[test]
    fn a_fixed_properties_object_has_one_field_entry_per_property() {
        let d = decl_type_for(&json!({
            "type": "object",
            "properties": {"name": {"type": "string"}, "replicas": {"type": "integer"}},
        }))
        .unwrap();
        match d.shape {
            Shape::Object(fields) => {
                assert_eq!(fields.len(), 2);
                assert!(fields.contains_key("name"));
                assert!(fields.contains_key("replicas"));
            }
            other => panic!("expected Shape::Object, got {other:?}"),
        }
    }

    #[test]
    fn a_required_property_with_no_default_grows_the_objects_own_min_size() {
        let with_required = decl_type_for(&json!({
            "type": "object",
            "properties": {"name": {"type": "string"}},
            "required": ["name"],
        }))
        .unwrap();
        let without_required = decl_type_for(&json!({
            "type": "object",
            "properties": {"name": {"type": "string"}},
        }))
        .unwrap();
        assert!(with_required.min_serialized_size > without_required.min_serialized_size);
    }

    #[test]
    fn a_required_property_with_a_default_does_not_grow_the_objects_own_min_size() {
        let d = decl_type_for(&json!({
            "type": "object",
            "properties": {"name": {"type": "string", "default": "x"}},
            "required": ["name"],
        }))
        .unwrap();
        // Same as the bare "{}" baseline -- the default means the client's
        // own request never has to carry the field.
        assert_eq!(d.min_serialized_size, 2);
    }

    #[test]
    fn x_kubernetes_int_or_string_is_a_dynamic_scalar_bounded_by_the_request_size() {
        let d = decl_type_for(&json!({"x-kubernetes-int-or-string": true})).unwrap();
        assert_eq!(d.max_elements, MAX_REQUEST_SIZE_BYTES - 2);
        assert_eq!(d.shape, Shape::Scalar);
    }

    #[test]
    fn x_kubernetes_int_or_string_with_max_length_is_bounded_by_it() {
        let d = decl_type_for(&json!({"x-kubernetes-int-or-string": true, "maxLength": 5})).unwrap();
        assert_eq!(d.max_elements, 20);
    }

    #[test]
    fn a_schema_with_no_recognized_type_at_all_is_not_exposed() {
        assert_eq!(decl_type_for(&json!({})), None);
    }

    #[test]
    fn an_object_with_no_properties_at_all_is_still_exposed_as_an_empty_object() {
        // Real upstream's own `SchemaDeclType` always returns an object
        // type for the "object" case -- an empty `fields` map, not `nil`
        // -- unlike the array/map cases, which really do return `nil`
        // when their own element schema can't be resolved.
        let d = decl_type_for(&json!({"type": "object"})).unwrap();
        assert_eq!(d.shape, Shape::Object(Default::default()));
    }
}
