//! Kubernetes' `kubernetes.Semver` CEL extension library.
//!
//! The opaque value and helpers below mirror upstream's semver library,
//! including strict parsing, the optional normalization argument, version
//! comparisons, and major/minor/patch accessors.

use super::kubernetes_quantity;
use cel::extractors::{Arguments, This};
use cel::objects::Opaque;
use cel::{ExecutionError, FunctionContext, Value};
use semver::Version;
use std::sync::Arc;

const SEMVER_TYPE: &str = "kubernetes.Semver";

#[derive(Debug, Clone)]
struct SemverValue(Version);

impl PartialEq for SemverValue {
    fn eq(&self, other: &Self) -> bool {
        self.0.cmp_precedence(&other.0) == std::cmp::Ordering::Equal
    }
}

impl Eq for SemverValue {}

impl Opaque for SemverValue {
    fn runtime_type_name(&self) -> &str {
        SEMVER_TYPE
    }
}

fn opaque(version: Version) -> Value {
    Value::Opaque(Arc::new(SemverValue(version)))
}

fn semver_ref(value: &Value) -> Option<&Version> {
    match value {
        Value::Opaque(value) => value.downcast_ref::<SemverValue>().map(|value| &value.0),
        _ => None,
    }
}

fn invalid_receiver(ftx: &FunctionContext, operation: &str) -> ExecutionError {
    ftx.error(format!("{operation}() requires a Kubernetes Semver"))
}

fn parse_version(raw: &str, normalize: bool) -> Result<Version, String> {
    if normalize {
        normalize_and_parse(raw)
    } else {
        Version::parse(raw).map_err(|error| error.to_string())
    }
}

fn normalize_and_parse(raw: &str) -> Result<Version, String> {
    let raw = raw.strip_prefix('v').unwrap_or(raw);
    let mut parts = raw.splitn(3, '.').map(str::to_string).collect::<Vec<_>>();
    for part in &mut parts {
        if part.len() > 1 {
            *part = part.trim_start_matches('0').to_string();
            if part.is_empty() || !part.as_bytes()[0].is_ascii_digit() {
                *part = format!("0{part}");
            }
        }
    }
    if parts.len() < 3 {
        if parts
            .last()
            .is_some_and(|part| part.contains('+') || part.contains('-'))
        {
            return Err("short version cannot contain PreRelease/Build meta data".to_string());
        }
        while parts.len() < 3 {
            parts.push("0".to_string());
        }
    }
    Version::parse(&parts.join(".")).map_err(|error| error.to_string())
}

pub fn semver_binding(
    ftx: &FunctionContext,
    Arguments(arguments): Arguments,
) -> Result<Value, ExecutionError> {
    match arguments.as_slice() {
        [Value::String(raw)] => parse_version(raw, false)
            .map(opaque)
            .map_err(|error| ftx.error(error)),
        [Value::String(raw), Value::Bool(normalize)] => parse_version(raw, *normalize)
            .map(opaque)
            .map_err(|error| ftx.error(error)),
        _ => {
            Err(ftx
                .error("semver() requires a string and an optional boolean normalization argument"))
        }
    }
}

pub fn is_semver_binding(
    ftx: &FunctionContext,
    Arguments(arguments): Arguments,
) -> Result<bool, ExecutionError> {
    match arguments.as_slice() {
        [Value::String(raw)] => Ok(parse_version(raw, false).is_ok()),
        [Value::String(raw), Value::Bool(normalize)] => Ok(parse_version(raw, *normalize).is_ok()),
        _ => Err(ftx
            .error("isSemver() requires a string and an optional boolean normalization argument")),
    }
}

fn version_operand<'a>(
    ftx: &FunctionContext,
    value: &'a Value,
    operation: &str,
) -> Result<&'a Version, ExecutionError> {
    semver_ref(value).ok_or_else(|| {
        ftx.error(format!(
            "{operation}() requires a Kubernetes Semver operand"
        ))
    })
}

pub fn is_greater_than_binding(
    ftx: &FunctionContext,
    This(value): This<Value>,
    operand: Value,
) -> Result<bool, ExecutionError> {
    if let Some(version) = semver_ref(&value) {
        return Ok(
            version.cmp_precedence(version_operand(ftx, &operand, "isGreaterThan")?)
                == std::cmp::Ordering::Greater,
        );
    }
    kubernetes_quantity::is_greater_than_binding(ftx, This(value), operand)
}

pub fn is_less_than_binding(
    ftx: &FunctionContext,
    This(value): This<Value>,
    operand: Value,
) -> Result<bool, ExecutionError> {
    if let Some(version) = semver_ref(&value) {
        return Ok(
            version.cmp_precedence(version_operand(ftx, &operand, "isLessThan")?)
                == std::cmp::Ordering::Less,
        );
    }
    kubernetes_quantity::is_less_than_binding(ftx, This(value), operand)
}

pub fn compare_to_binding(
    ftx: &FunctionContext,
    This(value): This<Value>,
    operand: Value,
) -> Result<i64, ExecutionError> {
    if let Some(version) = semver_ref(&value) {
        return Ok(
            match version.cmp_precedence(version_operand(ftx, &operand, "compareTo")?) {
                std::cmp::Ordering::Less => -1,
                std::cmp::Ordering::Equal => 0,
                std::cmp::Ordering::Greater => 1,
            },
        );
    }
    kubernetes_quantity::compare_to_binding(ftx, This(value), operand)
}

pub fn major_binding(
    ftx: &FunctionContext,
    This(value): This<Value>,
) -> Result<i64, ExecutionError> {
    semver_ref(&value)
        .map(|version| version.major as i64)
        .ok_or_else(|| invalid_receiver(ftx, "major"))
}

pub fn minor_binding(
    ftx: &FunctionContext,
    This(value): This<Value>,
) -> Result<i64, ExecutionError> {
    semver_ref(&value)
        .map(|version| version.minor as i64)
        .ok_or_else(|| invalid_receiver(ftx, "minor"))
}

pub fn patch_binding(
    ftx: &FunctionContext,
    This(value): This<Value>,
) -> Result<i64, ExecutionError> {
    semver_ref(&value)
        .map(|version| version.patch as i64)
        .ok_or_else(|| invalid_receiver(ftx, "patch"))
}

pub(crate) fn string_value(value: &Value) -> Option<String> {
    semver_ref(value).map(ToString::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strict_and_normalized_parsing_match_the_upstream_shapes() {
        assert!(parse_version("1.2.3", false).is_ok());
        assert!(parse_version("v1.2.3", false).is_err());
        assert_eq!(normalize_and_parse("v01.2").unwrap().to_string(), "1.2.0");
        assert!(normalize_and_parse("1.2-beta").is_err());
    }

    #[test]
    fn semantic_version_ordering_ignores_build_metadata() {
        let first = parse_version("1.2.3+one", false).unwrap();
        let second = parse_version("1.2.3+two", false).unwrap();
        assert_eq!(first.cmp_precedence(&second), std::cmp::Ordering::Equal);
    }
}
