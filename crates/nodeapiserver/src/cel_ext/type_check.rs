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
use cel::{IdedExpr, Program};
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

/// Check a rule at the top-level resource scope. Kubernetes exposes a small
/// set of identity and metadata fields at the root even when the CRD schema
/// does not repeat them; nested validation scopes do not get this addition.
pub fn check_root_rule(schema: &Value, rule: &str) -> Vec<TypeError> {
    check_rule_with_type(schema_type_for_root(schema), rule)
}

fn check_rule_with_type(root: Option<CelType>, rule: &str) -> Vec<TypeError> {
    let Some(root) = root else {
        return Vec::new();
    };
    let program = match Program::compile(rule) {
        Ok(program) => program,
        Err(error) => return vec![TypeError::Compile(error.to_string())],
    };
    let mut checker = Checker {
        variables: HashMap::from([
            (String::from("self"), root.clone()),
            (String::from("oldSelf"), root),
        ]),
        errors: Vec::new(),
    };
    let result = checker.expression(program.expression());
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

fn metadata_type() -> CelType {
    CelType::Object {
        fields: BTreeMap::from([
            ("name".to_string(), CelType::String),
            ("generateName".to_string(), CelType::String),
            ("namespace".to_string(), CelType::String),
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
                            (name.clone(), schema_type(value).unwrap_or(CelType::Dyn))
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

struct Checker {
    variables: HashMap<String, CelType>,
    errors: Vec<TypeError>,
}

impl Checker {
    fn expression(&mut self, expression: &IdedExpr) -> CelType {
        match &expression.expr {
            Expr::Unspecified => CelType::Dyn,
            Expr::Literal(literal) => literal_type(literal),
            Expr::Ident(name) => self.identifier(name),
            Expr::Select(select) => {
                let operand = self.expression(&select.operand);
                if select.test {
                    self.selected_type(operand, &select.field);
                    CelType::Bool
                } else {
                    self.selected_type(operand, &select.field)
                }
            }
            Expr::Call(call) => {
                let target = call.target.as_ref().map(|target| self.expression(target));
                let arguments = call
                    .args
                    .iter()
                    .map(|argument| self.expression(argument))
                    .collect::<Vec<_>>();
                self.call(&call.func_name, target, &arguments)
            }
            Expr::List(list) => {
                let mut element = CelType::Dyn;
                for item in &list.elements {
                    element = unify(element, self.expression(item));
                }
                CelType::List(Box::new(element))
            }
            Expr::Map(map) => {
                let mut value = CelType::Dyn;
                for entry in &map.entries {
                    match &entry.expr {
                        EntryExpr::MapEntry(entry) => {
                            self.expression(&entry.key);
                            value = unify(value, self.expression(&entry.value));
                        }
                        EntryExpr::StructField(field) => {
                            value = unify(value, self.expression(&field.value))
                        }
                    }
                }
                CelType::Map(Box::new(value))
            }
            Expr::Struct(structure) => {
                for entry in &structure.entries {
                    match &entry.expr {
                        EntryExpr::MapEntry(entry) => {
                            self.expression(&entry.key);
                            self.expression(&entry.value);
                        }
                        EntryExpr::StructField(field) => {
                            self.expression(&field.value);
                        }
                    }
                }
                CelType::Dyn
            }
            Expr::Comprehension(comprehension) => self.comprehension(comprehension),
        }
    }

    fn identifier(&mut self, name: &str) -> CelType {
        if let Some(ty) = self.variables.get(name) {
            return ty.clone();
        }
        // Qualified extension functions (`format.date()` and
        // `ip.isCanonical()`) use a namespace identifier that the runtime
        // resolves without looking up as a variable.
        if matches!(name, "format" | "ip") || name.starts_with('@') {
            return CelType::Dyn;
        }
        self.errors
            .push(TypeError::UnknownIdentifier(name.to_string()));
        CelType::Dyn
    }

    fn selected_type(&mut self, operand: CelType, field: &str) -> CelType {
        match operand {
            CelType::Object { fields, additional } => fields
                .get(field)
                .cloned()
                .or_else(|| additional.map(|value| *value))
                .unwrap_or_else(|| {
                    self.errors.push(TypeError::UnknownField {
                        field: field.to_string(),
                        on: CelType::Object {
                            fields,
                            additional: None,
                        },
                    });
                    CelType::Dyn
                }),
            CelType::Map(value) => *value,
            CelType::Dyn => CelType::Dyn,
            other => {
                self.errors.push(TypeError::InvalidOperand {
                    operation: "field selection".to_string(),
                    expected: "object or map".to_string(),
                    actual: other,
                });
                CelType::Dyn
            }
        }
    }

    fn call(&mut self, name: &str, target: Option<CelType>, arguments: &[CelType]) -> CelType {
        match name {
            operators::CONDITIONAL => {
                if let Some(condition) = arguments.first() {
                    self.require_bool(name, condition);
                }
                match (arguments.get(1), arguments.get(2)) {
                    (Some(left), Some(right)) => unify(left.clone(), right.clone()),
                    _ => CelType::Dyn,
                }
            }
            operators::LOGICAL_AND | operators::LOGICAL_OR => {
                for argument in arguments {
                    self.require_bool(name, argument);
                }
                CelType::Bool
            }
            operators::LOGICAL_NOT | operators::NOT_STRICTLY_FALSE => {
                if let Some(argument) = arguments.first() {
                    self.require_bool(name, argument);
                }
                CelType::Bool
            }
            operators::NEGATE => {
                if let Some(argument) = arguments.first() {
                    self.require_numeric(name, argument);
                    argument.clone()
                } else {
                    CelType::Dyn
                }
            }
            operators::ADD => self.binary_add(arguments),
            operators::SUBSTRACT | operators::MULTIPLY | operators::DIVIDE | operators::MODULO => {
                self.binary_numeric(name, arguments)
            }
            operators::GREATER
            | operators::GREATER_EQUALS
            | operators::LESS
            | operators::LESS_EQUALS => {
                self.binary_comparable(name, arguments);
                CelType::Bool
            }
            operators::EQUALS | operators::NOT_EQUALS => {
                if let (Some(left), Some(right)) = (arguments.first(), arguments.get(1)) {
                    if !compatible(left, right) {
                        self.errors.push(TypeError::IncompatibleOperands {
                            operation: name.to_string(),
                            left: left.clone(),
                            right: right.clone(),
                        });
                    }
                }
                CelType::Bool
            }
            operators::IN => {
                if let Some(container) = arguments.get(1) {
                    if !matches!(
                        container,
                        CelType::Dyn | CelType::List(_) | CelType::Map(_) | CelType::String
                    ) {
                        self.errors.push(TypeError::InvalidOperand {
                            operation: name.to_string(),
                            expected: "list, map, or string".to_string(),
                            actual: container.clone(),
                        });
                    }
                }
                CelType::Bool
            }
            operators::INDEX | operators::OPT_INDEX => self.index(arguments),
            "has" => CelType::Bool,
            "size" => {
                if let Some(value) = arguments.first().or(target.as_ref()) {
                    if !matches!(
                        value,
                        CelType::Dyn
                            | CelType::String
                            | CelType::Bytes
                            | CelType::List(_)
                            | CelType::Map(_)
                    ) {
                        self.errors.push(TypeError::InvalidOperand {
                            operation: "size".to_string(),
                            expected: "string, bytes, list, or map".to_string(),
                            actual: value.clone(),
                        });
                    }
                }
                CelType::Int
            }
            "contains" | "startsWith" | "endsWith" | "matches" => {
                self.string_method(name, target.as_ref(), arguments);
                CelType::Bool
            }
            "find" => {
                self.string_method(name, target.as_ref(), arguments);
                CelType::String
            }
            "findAll" => {
                self.string_method(name, target.as_ref(), arguments);
                CelType::List(Box::new(CelType::String))
            }
            "isSorted" | "includes" => {
                self.list_method(name, target.as_ref(), arguments);
                CelType::Bool
            }
            "min" | "max" | "sum" => {
                self.list_method(name, target.as_ref(), arguments);
                target
                    .and_then(|target| match target {
                        CelType::List(element) => Some(*element),
                        _ => None,
                    })
                    .unwrap_or(CelType::Dyn)
            }
            "indexOf" | "lastIndexOf" => {
                self.list_method(name, target.as_ref(), arguments);
                CelType::Int
            }
            "isQuantity" | "isIP" | "isCIDR" | "isURL" => {
                self.require_argument(name, arguments, 0, "string", is_string_type);
                CelType::Bool
            }
            "isSemver" => {
                self.require_argument(name, arguments, 0, "string", is_string_type);
                self.require_argument(name, arguments, 1, "bool", is_bool_type);
                CelType::Bool
            }
            "ip.isCanonical" => {
                self.require_argument(name, arguments, 0, "string", is_string_type);
                CelType::Bool
            }
            "quantity" => {
                if self.require_argument(name, arguments, 0, "string", is_string_type) {
                    CelType::Quantity
                } else {
                    CelType::Dyn
                }
            }
            "ip" => {
                if target.is_some() {
                    if self.require_receiver(name, target.as_ref(), "a CIDR", is_cidr_type) {
                        CelType::Ip
                    } else {
                        CelType::Dyn
                    }
                } else if self.require_argument(name, arguments, 0, "string", is_string_type) {
                    CelType::Ip
                } else {
                    CelType::Dyn
                }
            }
            "cidr" => {
                if self.require_argument(name, arguments, 0, "string", is_string_type) {
                    CelType::Cidr
                } else {
                    CelType::Dyn
                }
            }
            "url" => {
                if self.require_argument(name, arguments, 0, "string", is_string_type) {
                    CelType::Url
                } else {
                    CelType::Dyn
                }
            }
            "semver" => {
                let valid = self.require_argument(name, arguments, 0, "string", is_string_type);
                self.require_argument(name, arguments, 1, "bool", is_bool_type);
                if valid {
                    CelType::Semver
                } else {
                    CelType::Dyn
                }
            }
            "isInteger" => {
                self.require_receiver(name, target.as_ref(), "a Quantity", is_quantity_type);
                CelType::Bool
            }
            "asInteger" => {
                if self.require_receiver(name, target.as_ref(), "a Quantity", is_quantity_type) {
                    CelType::Int
                } else {
                    CelType::Dyn
                }
            }
            "asApproximateFloat" => {
                if self.require_receiver(name, target.as_ref(), "a Quantity", is_quantity_type) {
                    CelType::Double
                } else {
                    CelType::Dyn
                }
            }
            "sign" => {
                if self.require_receiver(name, target.as_ref(), "a Quantity", is_quantity_type) {
                    CelType::Int
                } else {
                    CelType::Dyn
                }
            }
            "add" | "sub" => {
                if self.require_receiver(name, target.as_ref(), "a Quantity", is_quantity_type) {
                    self.require_argument(
                        name,
                        arguments,
                        0,
                        "a Quantity or integer",
                        is_quantity_or_int_type,
                    );
                    CelType::Quantity
                } else {
                    CelType::Dyn
                }
            }
            "isLessThan" | "isGreaterThan" | "compareTo" => {
                self.comparison_method(name, target.as_ref(), arguments)
            }
            "family" => {
                if self.require_receiver(name, target.as_ref(), "an IP", is_ip_type) {
                    CelType::Int
                } else {
                    CelType::Dyn
                }
            }
            "isUnspecified" | "isLoopback" | "isLinkLocalMulticast" | "isLinkLocalUnicast"
            | "isGlobalUnicast" => {
                self.require_receiver(name, target.as_ref(), "an IP", is_ip_type);
                CelType::Bool
            }
            "prefixLength" => {
                if self.require_receiver(name, target.as_ref(), "a CIDR", is_cidr_type) {
                    CelType::Int
                } else {
                    CelType::Dyn
                }
            }
            "containsIP" => {
                if self.require_receiver(name, target.as_ref(), "a CIDR", is_cidr_type) {
                    self.require_argument(
                        name,
                        arguments,
                        0,
                        "an IP or string",
                        is_ip_or_string_type,
                    );
                    CelType::Bool
                } else {
                    CelType::Dyn
                }
            }
            "containsCIDR" => {
                if self.require_receiver(name, target.as_ref(), "a CIDR", is_cidr_type) {
                    self.require_argument(
                        name,
                        arguments,
                        0,
                        "a CIDR or string",
                        is_cidr_or_string_type,
                    );
                    CelType::Bool
                } else {
                    CelType::Dyn
                }
            }
            "masked" => {
                if self.require_receiver(name, target.as_ref(), "a CIDR", is_cidr_type) {
                    CelType::Cidr
                } else {
                    CelType::Dyn
                }
            }
            "getScheme" | "getHost" | "getHostname" | "getPort" | "getEscapedPath" => {
                if self.require_receiver(name, target.as_ref(), "a URL", is_url_type) {
                    CelType::String
                } else {
                    CelType::Dyn
                }
            }
            "getQuery" => {
                if self.require_receiver(name, target.as_ref(), "a URL", is_url_type) {
                    CelType::Map(Box::new(CelType::List(Box::new(CelType::String))))
                } else {
                    CelType::Dyn
                }
            }
            "major" | "minor" | "patch" => {
                if self.require_receiver(name, target.as_ref(), "a Semver", is_semver_type) {
                    CelType::Int
                } else {
                    CelType::Dyn
                }
            }
            "format.dns1123Label"
            | "format.dns1123Subdomain"
            | "format.dns1035Label"
            | "format.qualifiedName"
            | "format.dns1123LabelPrefix"
            | "format.dns1123SubdomainPrefix"
            | "format.dns1035LabelPrefix"
            | "format.labelValue"
            | "format.uri"
            | "format.uuid"
            | "format.byte"
            | "format.date"
            | "format.datetime" => CelType::Format,
            "format.named" => {
                self.require_argument(name, arguments, 0, "string", is_string_type);
                CelType::Optional(Box::new(CelType::Format))
            }
            "validate" => {
                if self.require_receiver(name, target.as_ref(), "a named format", is_format_type) {
                    self.require_argument(name, arguments, 0, "string", is_string_type);
                    CelType::Optional(Box::new(CelType::List(Box::new(CelType::String))))
                } else {
                    CelType::Dyn
                }
            }
            "hasValue" => {
                self.require_receiver(name, target.as_ref(), "an optional value", is_optional_type);
                CelType::Bool
            }
            "value" => match target.as_ref() {
                Some(CelType::Optional(value)) => (**value).clone(),
                Some(CelType::Dyn) | None => CelType::Dyn,
                Some(actual) => {
                    self.errors.push(TypeError::InvalidOperand {
                        operation: name.to_string(),
                        expected: "an optional value receiver".to_string(),
                        actual: actual.clone(),
                    });
                    CelType::Dyn
                }
            },
            "string" => {
                let value = target.as_ref().or_else(|| arguments.first());
                if let Some(value) = value {
                    if !matches!(value, CelType::Dyn | CelType::String | CelType::Int | CelType::Uint | CelType::Double | CelType::Bytes | CelType::Ip | CelType::Cidr | CelType::Url | CelType::Semver) {
                        self.errors.push(TypeError::InvalidOperand {
                            operation: name.to_string(),
                            expected: "a string-convertible value".to_string(),
                            actual: value.clone(),
                        });
                    }
                }
                CelType::String
            }
            "fieldSelector" | "labelSelector" | "group" | "resource" | "subresource"
            | "namespace" | "name" | "serviceAccount" | "check" | "allowed" | "errored"
            | "error" | "reason" => CelType::Dyn,
            _ if name.starts_with("format.") || name.starts_with("ip.") => CelType::Dyn,
            _ => CelType::Dyn,
        }
    }

    fn require_receiver(
        &mut self,
        operation: &str,
        target: Option<&CelType>,
        expected: &str,
        predicate: fn(&CelType) -> bool,
    ) -> bool {
        match target {
            Some(CelType::Dyn) => true,
            Some(actual) if predicate(actual) => true,
            Some(actual) => {
                self.errors.push(TypeError::InvalidOperand {
                    operation: operation.to_string(),
                    expected: format!("{expected} receiver"),
                    actual: actual.clone(),
                });
                false
            }
            None => false,
        }
    }

    fn require_argument(
        &mut self,
        operation: &str,
        arguments: &[CelType],
        index: usize,
        expected: &str,
        predicate: fn(&CelType) -> bool,
    ) -> bool {
        match arguments.get(index) {
            Some(CelType::Dyn) => true,
            Some(actual) if predicate(actual) => true,
            Some(actual) => {
                self.errors.push(TypeError::InvalidOperand {
                    operation: operation.to_string(),
                    expected: expected.to_string(),
                    actual: actual.clone(),
                });
                false
            }
            None => false,
        }
    }

    fn comparison_method(
        &mut self,
        name: &str,
        target: Option<&CelType>,
        arguments: &[CelType],
    ) -> CelType {
        let Some(target) = target else {
            return CelType::Dyn;
        };
        let valid = match target {
            CelType::Dyn => true,
            CelType::Quantity => self.require_argument(
                name,
                arguments,
                0,
                "a Quantity",
                is_quantity_type,
            ),
            CelType::Semver => self.require_argument(
                name,
                arguments,
                0,
                "a Semver",
                is_semver_type,
            ),
            actual => {
                self.errors.push(TypeError::InvalidOperand {
                    operation: name.to_string(),
                    expected: "a Quantity or Semver receiver".to_string(),
                    actual: actual.clone(),
                });
                false
            }
        };
        if valid {
            if name == "compareTo" {
                CelType::Int
            } else {
                CelType::Bool
            }
        } else {
            CelType::Dyn
        }
    }

    fn binary_add(&mut self, arguments: &[CelType]) -> CelType {
        let (Some(left), Some(right)) = (arguments.first(), arguments.get(1)) else {
            return CelType::Dyn;
        };
        if left.is_dynamic() || right.is_dynamic() {
            return CelType::Dyn;
        }
        if matches!((left, right), (CelType::String, CelType::String)) {
            return CelType::String;
        }
        if let (CelType::List(left), CelType::List(right)) = (left, right) {
            return CelType::List(Box::new(unify((**left).clone(), (**right).clone())));
        }
        if left.is_numeric() && right.is_numeric() {
            return unify(left.clone(), right.clone());
        }
        self.errors.push(TypeError::IncompatibleOperands {
            operation: operators::ADD.to_string(),
            left: left.clone(),
            right: right.clone(),
        });
        CelType::Dyn
    }

    fn binary_numeric(&mut self, name: &str, arguments: &[CelType]) -> CelType {
        let (Some(left), Some(right)) = (arguments.first(), arguments.get(1)) else {
            return CelType::Dyn;
        };
        if left.is_dynamic() || right.is_dynamic() {
            return CelType::Dyn;
        }
        if left.is_numeric() && right.is_numeric() {
            return unify(left.clone(), right.clone());
        }
        self.errors.push(TypeError::IncompatibleOperands {
            operation: name.to_string(),
            left: left.clone(),
            right: right.clone(),
        });
        CelType::Dyn
    }

    fn binary_comparable(&mut self, name: &str, arguments: &[CelType]) {
        let (Some(left), Some(right)) = (arguments.first(), arguments.get(1)) else {
            return;
        };
        if left.is_dynamic()
            || right.is_dynamic()
            || compatible(left, right) && (left.is_numeric() && right.is_numeric() || left == right)
        {
            return;
        }
        self.errors.push(TypeError::IncompatibleOperands {
            operation: name.to_string(),
            left: left.clone(),
            right: right.clone(),
        });
    }

    fn index(&mut self, arguments: &[CelType]) -> CelType {
        let (Some(container), Some(index)) = (arguments.first(), arguments.get(1)) else {
            return CelType::Dyn;
        };
        match container {
            CelType::Dyn => CelType::Dyn,
            CelType::List(element) => {
                if !matches!(index, CelType::Dyn | CelType::Int | CelType::Uint) {
                    self.errors.push(TypeError::InvalidOperand {
                        operation: "list index".to_string(),
                        expected: "int or uint".to_string(),
                        actual: index.clone(),
                    });
                }
                (**element).clone()
            }
            CelType::Map(value) => {
                if !matches!(index, CelType::Dyn | CelType::String) {
                    self.errors.push(TypeError::InvalidOperand {
                        operation: "map index".to_string(),
                        expected: "string".to_string(),
                        actual: index.clone(),
                    });
                }
                (**value).clone()
            }
            other => {
                self.errors.push(TypeError::InvalidOperand {
                    operation: "index".to_string(),
                    expected: "list or map".to_string(),
                    actual: other.clone(),
                });
                CelType::Dyn
            }
        }
    }

    fn string_method(&mut self, name: &str, target: Option<&CelType>, arguments: &[CelType]) {
        if let Some(target) = target {
            if !matches!(target, CelType::Dyn | CelType::String) {
                self.errors.push(TypeError::InvalidOperand {
                    operation: name.to_string(),
                    expected: "string receiver".to_string(),
                    actual: target.clone(),
                });
            }
        }
        if let Some(pattern) = arguments.first() {
            if !matches!(pattern, CelType::Dyn | CelType::String) {
                self.errors.push(TypeError::InvalidOperand {
                    operation: name.to_string(),
                    expected: "string argument".to_string(),
                    actual: pattern.clone(),
                });
            }
        }
        if name == "findAll" {
            if let Some(limit) = arguments.get(1) {
                if !matches!(limit, CelType::Dyn | CelType::Int) {
                    self.errors.push(TypeError::InvalidOperand {
                        operation: name.to_string(),
                        expected: "an optional int limit".to_string(),
                        actual: limit.clone(),
                    });
                }
            }
        }
    }

    fn list_method(&mut self, name: &str, target: Option<&CelType>, _arguments: &[CelType]) {
        if let Some(target) = target {
            if !matches!(target, CelType::Dyn | CelType::List(_)) {
                self.errors.push(TypeError::InvalidOperand {
                    operation: name.to_string(),
                    expected: "list receiver".to_string(),
                    actual: target.clone(),
                });
            }
        }
    }

    fn require_bool(&mut self, operation: &str, actual: &CelType) {
        if !actual.is_bool_or_dynamic() {
            self.errors.push(TypeError::InvalidOperand {
                operation: operation.to_string(),
                expected: "bool".to_string(),
                actual: actual.clone(),
            });
        }
    }

    fn require_numeric(&mut self, operation: &str, actual: &CelType) {
        if !actual.is_dynamic() && !actual.is_numeric() {
            self.errors.push(TypeError::InvalidOperand {
                operation: operation.to_string(),
                expected: "a number".to_string(),
                actual: actual.clone(),
            });
        }
    }

    fn comprehension(&mut self, comprehension: &cel::common::ast::ComprehensionExpr) -> CelType {
        let range = self.expression(&comprehension.iter_range);
        let element = match range {
            CelType::List(element) => *element,
            CelType::Map(value) => *value,
            CelType::Dyn => CelType::Dyn,
            other => {
                self.errors.push(TypeError::InvalidOperand {
                    operation: "comprehension".to_string(),
                    expected: "list or map range".to_string(),
                    actual: other,
                });
                CelType::Dyn
            }
        };
        let previous_iter = self
            .variables
            .insert(comprehension.iter_var.clone(), element.clone());
        let previous_iter2 = comprehension
            .iter_var2
            .as_ref()
            .map(|name| self.variables.insert(name.clone(), CelType::String));
        let accumulator = self.expression(&comprehension.accu_init);
        let previous_accumulator = self
            .variables
            .insert(comprehension.accu_var.clone(), accumulator);
        let condition = self.expression(&comprehension.loop_cond);
        self.require_bool("comprehension condition", &condition);
        self.expression(&comprehension.loop_step);
        let result = self.expression(&comprehension.result);
        restore(&mut self.variables, &comprehension.iter_var, previous_iter);
        if let Some(name) = &comprehension.iter_var2 {
            restore(
                &mut self.variables,
                name,
                previous_iter2.expect("iter_var2 was inserted"),
            );
        }
        restore(
            &mut self.variables,
            &comprehension.accu_var,
            previous_accumulator,
        );
        result
    }
}

fn is_bool_type(value: &CelType) -> bool {
    matches!(value, CelType::Bool)
}

fn is_string_type(value: &CelType) -> bool {
    matches!(value, CelType::String)
}

fn is_quantity_type(value: &CelType) -> bool {
    matches!(value, CelType::Quantity)
}

fn is_ip_type(value: &CelType) -> bool {
    matches!(value, CelType::Ip)
}

fn is_cidr_type(value: &CelType) -> bool {
    matches!(value, CelType::Cidr)
}

fn is_url_type(value: &CelType) -> bool {
    matches!(value, CelType::Url)
}

fn is_semver_type(value: &CelType) -> bool {
    matches!(value, CelType::Semver)
}

fn is_format_type(value: &CelType) -> bool {
    matches!(value, CelType::Format)
}

fn is_optional_type(value: &CelType) -> bool {
    matches!(value, CelType::Optional(_))
}

fn is_quantity_or_int_type(value: &CelType) -> bool {
    matches!(value, CelType::Quantity | CelType::Int)
}

fn is_ip_or_string_type(value: &CelType) -> bool {
    matches!(value, CelType::Ip | CelType::String)
}

fn is_cidr_or_string_type(value: &CelType) -> bool {
    matches!(value, CelType::Cidr | CelType::String)
}

fn restore(variables: &mut HashMap<String, CelType>, name: &str, previous: Option<CelType>) {
    if let Some(previous) = previous {
        variables.insert(name.to_string(), previous);
    } else {
        variables.remove(name);
    }
}

fn literal_type(literal: &LiteralValue) -> CelType {
    match literal {
        LiteralValue::Boolean(_) => CelType::Bool,
        LiteralValue::Bytes(_) => CelType::Bytes,
        LiteralValue::Double(_) => CelType::Double,
        LiteralValue::Int(_) => CelType::Int,
        LiteralValue::Null => CelType::Null,
        LiteralValue::String(_) => CelType::String,
        LiteralValue::UInt(_) => CelType::Uint,
    }
}

fn compatible(left: &CelType, right: &CelType) -> bool {
    left.is_dynamic()
        || right.is_dynamic()
        || matches!(left, CelType::Null)
        || matches!(right, CelType::Null)
        || left == right
        || left.is_numeric() && right.is_numeric()
}

fn unify(left: CelType, right: CelType) -> CelType {
    if left.is_dynamic() {
        return right;
    }
    if right.is_dynamic() || left == right {
        return left;
    }
    if left.is_numeric() && right.is_numeric() {
        if matches!(left, CelType::Double) || matches!(right, CelType::Double) {
            CelType::Double
        } else if matches!(left, CelType::Uint) && matches!(right, CelType::Uint) {
            CelType::Uint
        } else {
            CelType::Int
        }
    } else {
        CelType::Dyn
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn schema() -> Value {
        json!({
            "type": "object",
            "properties": {
                "name": {"type": "string"},
                "replicas": {"type": "integer"},
                "tags": {"type": "array", "items": {"type": "object", "properties": {"name": {"type": "string"}}}},
            },
        })
    }

    #[test]
    fn declared_fields_and_nested_comprehension_variables_are_typed() {
        assert!(check_rule(
            &schema(),
            "self.replicas > 0 && self.tags.all(tag, tag.name != '')"
        )
        .is_empty());
    }

    #[test]
    fn an_undeclared_field_is_rejected() {
        let errors = check_rule(&schema(), "self.missing == 'x'");
        assert!(errors.iter().any(
            |error| matches!(error, TypeError::UnknownField { field, .. } if field == "missing")
        ));
    }

    #[test]
    fn an_obvious_operand_mismatch_is_rejected() {
        let errors = check_rule(&schema(), "self.name + 1");
        assert!(errors
            .iter()
            .any(|error| matches!(error, TypeError::IncompatibleOperands { .. })));
    }

    #[test]
    fn validation_rules_must_be_boolean() {
        let errors = check_rule(&schema(), "self.name");
        assert!(errors
            .iter()
            .any(|error| matches!(error, TypeError::NonBoolean(CelType::String))));
    }

    #[test]
    fn root_metadata_is_available_even_when_the_crd_schema_omits_it() {
        assert!(check_root_rule(
            &schema(),
            "self.metadata.name != '' && self.apiVersion != ''"
        )
        .is_empty());
    }

    #[test]
    fn dynamic_schema_sections_do_not_produce_false_positive_field_errors() {
        let schema = json!({"type": "object", "properties": {"data": {"type": "object", "x-kubernetes-preserve-unknown-fields": true}}});
        assert!(check_rule(&schema, "self.data.anything == 1").is_empty());
    }

    #[test]
    fn kubernetes_extension_values_and_format_optionals_are_typed() {
        let rule = "quantity(self.name).add(1).isGreaterThan(quantity('0')) && cidr('10.0.0.0/8').containsIP(ip('10.1.2.3')) && ip.isCanonical(self.name) && url(self.name).getQuery()['key'][0] == 'value' && semver('1.2.3').major() == 1 && format.named('uuid').value().validate(self.name).hasValue()";
        assert!(check_rule(&schema(), rule).is_empty());
    }

    #[test]
    fn kubernetes_extension_overloads_reject_wrong_operands() {
        let errors = check_rule(
            &schema(),
            "quantity(self.name).add('1') == quantity('2') || ip(self.name).family() == '4' || url(self.name).getHostname().family() == 4",
        );
        assert!(errors.iter().any(|error| matches!(
            error,
            TypeError::InvalidOperand { operation, .. } if operation == "add"
        )));
        assert!(errors.iter().any(|error| matches!(
            error,
            TypeError::IncompatibleOperands { .. }
        )));
        assert!(errors.iter().any(|error| matches!(
            error,
            TypeError::InvalidOperand { operation, .. } if operation == "family"
        )));
    }

    #[test]
    fn kubernetes_extension_constructors_require_strings() {
        let errors = check_rule(&schema(), "quantity(self.replicas) == quantity('1')");
        assert!(errors.iter().any(|error| matches!(
            error,
            TypeError::InvalidOperand { operation, .. } if operation == "quantity"
        )));
    }
}
