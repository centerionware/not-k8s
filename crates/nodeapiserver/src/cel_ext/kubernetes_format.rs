//! Kubernetes' `kubernetes.format` CEL extension library.
//!
//! Formats are opaque values so the same validator can be selected by a
//! convenience function (`format.dns1123Label()`) or by
//! `format.named("dns1123Label")`. Validation returns cel-rust's native
//! optional value: none for a valid string and a list of messages otherwise.

use cel::extractors::This;
use cel::objects::{Opaque, OptionalValue};
use cel::{ExecutionError, FunctionContext, Value};
use std::sync::Arc;

use super::super::scheme::name_format;

const FORMAT_TYPE: &str = "kubernetes.NamedFormat";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FormatValue(&'static str);

impl Opaque for FormatValue {
    fn runtime_type_name(&self) -> &str {
        FORMAT_TYPE
    }
}

#[derive(Clone, Copy)]
struct FormatSpec {
    name: &'static str,
    validate: fn(&str) -> Vec<String>,
}

fn specs() -> &'static [FormatSpec] {
    &[
        FormatSpec { name: "dns1123Label", validate: name_format::is_dns1123_label },
        FormatSpec { name: "dns1123Subdomain", validate: name_format::is_dns1123_subdomain },
        FormatSpec { name: "dns1035Label", validate: name_format::is_dns1035_label },
        FormatSpec { name: "qualifiedName", validate: validate_qualified_name },
        FormatSpec { name: "dns1123LabelPrefix", validate: validate_dns1123_label_prefix },
        FormatSpec { name: "dns1123SubdomainPrefix", validate: validate_dns1123_subdomain_prefix },
        FormatSpec { name: "dns1035LabelPrefix", validate: validate_dns1035_label_prefix },
        FormatSpec { name: "labelValue", validate: validate_label_value },
        FormatSpec { name: "uri", validate: validate_uri },
        FormatSpec { name: "uuid", validate: validate_uuid },
        FormatSpec { name: "byte", validate: validate_byte },
        FormatSpec { name: "date", validate: validate_date },
        FormatSpec { name: "datetime", validate: validate_datetime },
    ]
}

fn spec(name: &str) -> Option<FormatSpec> {
    specs().iter().copied().find(|spec| spec.name == name)
}

fn format_value(name: &'static str) -> Value {
    Value::Opaque(Arc::new(FormatValue(name)))
}

fn optional(value: Option<Value>) -> Value {
    Value::Opaque(Arc::new(match value {
        Some(value) => OptionalValue::of(value),
        None => OptionalValue::none(),
    }))
}

fn format_ref(value: &Value) -> Option<&'static str> {
    match value {
        Value::Opaque(value) => value.downcast_ref::<FormatValue>().map(|value| value.0),
        _ => None,
    }
}

pub fn named_binding(name: Arc<String>) -> Value {
    match spec(&name) {
        Some(spec) => optional(Some(format_value(spec.name))),
        None => optional(None),
    }
}

pub fn validate_binding(
    ftx: &FunctionContext,
    This(format): This<Value>,
    value: Arc<String>,
) -> Result<Value, ExecutionError> {
    let name = format_ref(&format).ok_or_else(|| ftx.error("validate() requires a Kubernetes format"))?;
    let spec = spec(name).expect("every FormatValue is created from a known format");
    let errors = (spec.validate)(&value);
    Ok(optional(if errors.is_empty() {
        None
    } else {
        Some(Value::List(Arc::new(
            errors
                .into_iter()
                .map(|error| Value::String(Arc::new(error)))
                .collect(),
        )))
    }))
}

macro_rules! format_binding {
    ($name:ident, $format:literal) => {
        pub fn $name() -> Value {
            format_value($format)
        }
    };
}

format_binding!(dns1123_label_binding, "dns1123Label");
format_binding!(dns1123_subdomain_binding, "dns1123Subdomain");
format_binding!(dns1035_label_binding, "dns1035Label");
format_binding!(qualified_name_binding, "qualifiedName");
format_binding!(dns1123_label_prefix_binding, "dns1123LabelPrefix");
format_binding!(dns1123_subdomain_prefix_binding, "dns1123SubdomainPrefix");
format_binding!(dns1035_label_prefix_binding, "dns1035LabelPrefix");
format_binding!(label_value_binding, "labelValue");
format_binding!(uri_binding, "uri");
format_binding!(uuid_binding, "uuid");
format_binding!(byte_binding, "byte");
format_binding!(date_binding, "date");
format_binding!(datetime_binding, "datetime");

fn prefix_variant(value: &str, validate: fn(&str) -> Vec<String>) -> Vec<String> {
    if value.len() > 1 && value.ends_with('-') {
        let mut masked = value[..value.len() - 1].to_string();
        masked.push('a');
        validate(&masked)
    } else {
        validate(value)
    }
}

fn validate_dns1123_label_prefix(value: &str) -> Vec<String> {
    prefix_variant(value, name_format::is_dns1123_label)
}

fn validate_dns1123_subdomain_prefix(value: &str) -> Vec<String> {
    prefix_variant(value, name_format::is_dns1123_subdomain)
}

fn validate_dns1035_label_prefix(value: &str) -> Vec<String> {
    prefix_variant(value, name_format::is_dns1035_label)
}

fn validate_label_value(value: &str) -> Vec<String> {
    if value.len() > 63 {
        return vec!["must be no more than 63 characters".to_string()];
    }
    if value.is_empty() {
        return Vec::new();
    }
    let bytes = value.as_bytes();
    if bytes[0].is_ascii_alphanumeric()
        && bytes[bytes.len() - 1].is_ascii_alphanumeric()
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(*byte, b'-' | b'_' | b'.'))
    {
        Vec::new()
    } else {
        vec!["must be an alphanumeric string with '-', '_' or '.'".to_string()]
    }
}

fn validate_qualified_name(value: &str) -> Vec<String> {
    let Some((prefix, name)) = value.split_once('/') else {
        return validate_label_value(value);
    };
    if value.matches('/').count() != 1 {
        return vec!["must contain a single '/' separator".to_string()];
    }
    let mut errors = name_format::is_dns1123_subdomain(prefix);
    errors.extend(validate_label_value(name));
    errors
}

fn validate_uri(value: &str) -> Vec<String> {
    if url::Url::parse(value).is_ok() {
        Vec::new()
    } else {
        vec!["invalid URI".to_string()]
    }
}

fn validate_uuid(value: &str) -> Vec<String> {
    if uuid::Uuid::parse_str(value).is_ok() {
        Vec::new()
    } else {
        vec!["does not match the UUID format".to_string()]
    }
}

fn validate_byte(value: &str) -> Vec<String> {
    use base64::Engine;
    if base64::engine::general_purpose::STANDARD.decode(value).is_ok() {
        Vec::new()
    } else {
        vec!["invalid base64".to_string()]
    }
}

fn validate_date(value: &str) -> Vec<String> {
    if chrono::NaiveDate::parse_from_str(value, "%Y-%m-%d").is_ok() {
        Vec::new()
    } else {
        vec!["invalid date".to_string()]
    }
}

fn validate_datetime(value: &str) -> Vec<String> {
    if chrono::DateTime::parse_from_rfc3339(value).is_ok() {
        Vec::new()
    } else {
        vec!["invalid datetime".to_string()]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_and_crd_formats_accept_and_reject_expected_values() {
        assert!(validate_qualified_name("example.com/widget").is_empty());
        assert!(!validate_qualified_name("Example.com/widget").is_empty());
        assert!(validate_label_value("").is_empty());
        assert!(validate_uuid("123e4567-e89b-12d3-a456-426614174000").is_empty());
        assert!(!validate_byte("not base64").is_empty());
        assert!(validate_date("2021-01-01").is_empty());
        assert!(validate_datetime("2021-01-01T00:00:00Z").is_empty());
    }
}
