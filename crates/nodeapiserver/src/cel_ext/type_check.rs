//! Schema-aware checking for CRD `x-kubernetes-validations` expressions.
//!
//! The `cel` crate intentionally provides parsing and runtime overload
//! dispatch, not the declaration/type-checking phase Kubernetes performs
//! when a CRD is accepted. This module supplies the small schema-driven part
//! needed by CRD validation: it resolves `self`/`oldSelf` against the local
//! structural schema, rejects fields that schema does not expose, checks the
//! obvious operator and member-function overloads, and requires a boolean
//! result. Unknown schema portions remain dynamic, matching Kubernetes'
//! treatment of data whose type cannot be declared structurally.

use cel::common::ast::{operators, EntryExpr, Expr, LiteralValue};
use cel::IdedExpr;
use serde_json::Value;
use std::collections::{BTreeMap, HashMap};
use std::fmt::{Display, Formatter};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CelType {
    Dyn,
    Null,
    Bool,
    Int,
    Uint,
    Double,
    String,
    Bytes,
    List(Box<Self>),
    Map(Box<Self>),
    Quantity,
    Ip,
    Cidr,
    Url,
    Semver,
    Format,
    Optional(Box<Self>),
    Object {
        fields: BTreeMap<String, Self>,
        additional: Option<Box<Self>>,
    },
}

impl CelType {
    fn is_dynamic(&self) -> bool {
        matches!(self, Self::Dyn)
    }

    fn is_numeric(&self) -> bool {
        matches!(self, Self::Int | Self::Uint | Self::Double)
    }

    fn is_bool_or_dynamic(&self) -> bool {
        matches!(self, Self::Bool | Self::Dyn)
    }
}

impl Display for CelType {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Dyn => f.write_str("dyn"),
            Self::Null => f.write_str("null"),
            Self::Bool => f.write_str("bool"),
            Self::Int => f.write_str("int"),
            Self::Uint => f.write_str("uint"),
            Self::Double => f.write_str("double"),
            Self::String => f.write_str("string"),
            Self::Bytes => f.write_str("bytes"),
            Self::List(element) => write!(f, "list({element})"),
            Self::Map(value) => write!(f, "map(string, {value})"),
            Self::Quantity => f.write_str("kubernetes.Quantity"),
            Self::Ip => f.write_str("net.IP"),
            Self::Cidr => f.write_str("net.CIDR"),
            Self::Url => f.write_str("kubernetes.URL"),
            Self::Semver => f.write_str("kubernetes.Semver"),
            Self::Format => f.write_str("kubernetes.NamedFormat"),
            Self::Optional(value) => write!(f, "optional({value})"),
            Self::Object { .. } => f.write_str("object"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TypeError {
    Compile(String),
    UnknownIdentifier(String),
    UnknownField {
        field: String,
        on: CelType,
    },
    InvalidOperand {
        operation: String,
        expected: String,
        actual: CelType,
    },
    IncompatibleOperands {
        operation: String,
        left: CelType,
        right: CelType,
    },
    NonBoolean(CelType),
}

impl Display for TypeError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Compile(error) => write!(f, "compilation failed: {error}"),
            Self::UnknownIdentifier(name) => write!(f, "undeclared identifier {name:?}"),
            Self::UnknownField { field, on } => {
                write!(f, "field {field:?} is not declared on {on}")
            }
            Self::InvalidOperand {
                operation,
                expected,
                actual,
            } => write!(f, "{operation} requires {expected}, got {actual}"),
            Self::IncompatibleOperands {
                operation,
                left,
                right,
            } => write!(f, "{operation} cannot combine {left} and {right}"),
            Self::NonBoolean(actual) => {
                write!(f, "validation rule must evaluate to bool, got {actual}")
            }
        }
    }
}

/// Check one rule against the schema at the rule's own scope.
pub fn check_rule(schema: &Value, rule: &str) -> Vec<TypeError> {
    check_rule_with_type(schema_type(schema), rule)
}

/// Check a rule against the schema while modeling Kubernetes' optional
/// `oldSelf` binding used by rules with `optionalOldSelf: true`.
pub fn check_rule_with_optional_old_self(
    schema: &Value,
    rule: &str,
    optional_old_self: bool,
) -> Vec<TypeError> {
    let root = schema_type(schema);
    let old_self = root.clone().map(|root| {
        if optional_old_self {
            CelType::Optional(Box::new(root))
        } else {
            root
        }
    });
    check_rule_with_old_self(root, old_self, rule)
}

/// Check a rule at the top-level resource scope. Kubernetes exposes a small
/// set of identity and metadata fields at the root even when the CRD schema
/// does not repeat them; nested validation scopes do not get this addition.
pub fn check_root_rule(schema: &Value, rule: &str) -> Vec<TypeError> {
    check_rule_with_type(schema_type_for_root(schema), rule)
}

/// Check a top-level rule with the optional `oldSelf` type used when its
/// `optionalOldSelf` field is enabled.
pub fn check_root_rule_with_optional_old_self(
    schema: &Value,
    rule: &str,
    optional_old_self: bool,
) -> Vec<TypeError> {
    let root = schema_type_for_root(schema);
    let old_self = root.clone().map(|root| {
        if optional_old_self {
            CelType::Optional(Box::new(root))
        } else {
            root
        }
    });
    check_rule_with_old_self(root, old_self, rule)
}

/// Return whether a rule actually references the `oldSelf` identifier. This
/// distinguishes ordinary rules, which still run on CREATE, from transition
/// rules that Kubernetes skips when no prior value exists unless they opt
/// into optional-old-self behavior.
pub fn rule_references_old_self(rule: &str) -> bool {
    let Ok(expression) = super::compile(rule) else {
        return false;
    };
    let mut checker = Checker {
        variables: HashMap::new(),
        errors: Vec::new(),
        references_old_self: false,
    };
    checker.expression(&expression);
    checker.references_old_self
}

fn check_rule_with_type(root: Option<CelType>, rule: &str) -> Vec<TypeError> {
    check_rule_with_old_self(root.clone(), root, rule)
}

/// Check a rule with an explicitly declared `oldSelf` type. The regular
/// helper keeps the historical non-optional binding; CRD rules opt into the
/// optional form per rule through `check_*_with_optional_old_self`.
fn check_rule_with_old_self(
    root: Option<CelType>,
    old_self: Option<CelType>,
    rule: &str,
) -> Vec<TypeError> {
    let Some(root) = root else {
        return Vec::new();
    };
    let expression = match super::compile(rule) {
        Ok(expression) => expression,
        Err(error) => return vec![TypeError::Compile(error.to_string())],
    };
    let mut checker = Checker {
        variables: HashMap::from([
            (String::from("self"), root.clone()),
            (String::from("oldSelf"), old_self.unwrap_or(root)),
        ]),
        errors: Vec::new(),
        references_old_self: false,
    };
    let result = checker.expression(&expression);
    if !result.is_bool_or_dynamic() {
        checker.errors.push(TypeError::NonBoolean(result));
    }
    checker.errors
}

/// Convert an OpenAPI structural schema into the CEL type visible at one
/// validation scope. The root adds the metadata fields Kubernetes exposes
/// even when they are not repeated in a CRD's user schema.
pub fn schema_type_for_root(schema: &Value) -> Option<CelType> {
    let mut root = schema_type(schema)?;
    let CelType::Object { fields, .. } = &mut root else {
        return Some(root);
    };
    fields
        .entry("apiVersion".to_string())
        .or_insert(CelType::String);
    fields.entry("kind".to_string()).or_insert(CelType::String);
    fields
        .entry("metadata".to_string())
        .or_insert_with(metadata_type);
    Some(root)
}

/// Return the CEL identifier Kubernetes exposes for an OpenAPI property.
pub fn cel_field_name(name: &str) -> String {
    const RESERVED: &[&str] = &[
        "as", "break", "const", "continue", "else", "for", "function", "if", "import",
        "in", "let", "loop", "namespace", "package", "return", "true", "false", "null",
    ];
    if RESERVED.contains(&name) {
        return format!("__{name}__");
    }
    name.replace("__", "__underscores__")
        .replace('.', "__dot__")
        .replace('-', "__dash__")
        .replace('/', "__slash__")
}

fn metadata_type() -> CelType {
    CelType::Object {
        fields: BTreeMap::from([
            ("name".to_string(), CelType::String),
            ("generateName".to_string(), CelType::String),
            (cel_field_name("namespace"), CelType::String),
            (
                "labels".to_string(),
                CelType::Map(Box::new(CelType::String)),
            ),
            (
                "annotations".to_string(),
                CelType::Map(Box::new(CelType::String)),
            ),
        ]),
        additional: Some(Box::new(CelType::Dyn)),
    }
}

fn schema_type(schema: &Value) -> Option<CelType> {
    if schema
        .get("x-kubernetes-int-or-string")
        .and_then(Value::as_bool)
        == Some(true)
    {
        return Some(CelType::Dyn);
    }
    match schema.get("type").and_then(Value::as_str) {
        Some("boolean") => Some(CelType::Bool),
        Some("integer") => Some(CelType::Int),
        Some("number") => Some(CelType::Double),
        Some("string") => Some(CelType::String),
        Some("array") => Some(CelType::List(Box::new(
            schema
                .get("items")
                .and_then(schema_type)
                .unwrap_or(CelType::Dyn),
        ))),
        Some("object") => {
            let fields: BTreeMap<String, CelType> = schema
                .get("properties")
                .and_then(Value::as_object)
                .map(|properties| {
                    properties
                        .iter()
                        .map(|(name, value)| {
                            (cel_field_name(name), schema_type(value).unwrap_or(CelType::Dyn))
                        })
                        .collect()
                })
                .unwrap_or_default();
            let additional = schema
                .get("additionalProperties")
                .filter(|value| value.is_object())
                .and_then(schema_type)
                .map(Box::new);
            if fields.is_empty() && additional.is_none() {
                Some(CelType::Dyn)
            } else {
                Some(CelType::Object { fields, additional })
            }
        }
        // A schema without a declared type is dynamic to CEL. Structural
        // validation remains responsible for rejecting malformed schemas.
        _ => Some(CelType::Dyn),
    }
}

include!("type_check/checker.rs");
include!("type_check/helpers.rs");
include!("type_check/tests.rs");
