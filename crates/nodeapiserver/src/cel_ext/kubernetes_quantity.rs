//! Real upstream's own `kubernetes.quantity` CEL extension library
//! (`k8s.io/apiserver/pkg/cel/library/quantity.go`, fetched and read
//! directly) — `docs/APISERVER.md`'s own Group F quantity/CEL work. The
//! parser-backed opaque value below provides the real
//! `quantity(<string>) <Quantity>` constructor and member functions
//! (`isInteger`/`asInteger`/`asApproximateFloat`/`sign`/`add`/`sub`/
//! `isLessThan`/`isGreaterThan`/`compareTo`).
//!
//! [`is_quantity`] and [`quantity`] reuse this crate's own
//! [`crate::scheme::quantity::Quantity::parse`] rather than reimplementing
//! quantity parsing a second time. `isQuantity` is real upstream's own
//! "does `quantity` not error" definition exactly, so both bindings share
//! one parser and cannot silently diverge.

use crate::scheme::quantity::Quantity;
use cel::extractors::This;
use cel::objects::Opaque;
use cel::{ExecutionError, FunctionContext, Value};
use std::sync::Arc;

const QUANTITY_TYPE: &str = "kubernetes.Quantity";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct QuantityValue(Quantity);

impl Opaque for QuantityValue {
    fn runtime_type_name(&self) -> &str {
        QUANTITY_TYPE
    }
}

fn opaque(quantity: Quantity) -> Value {
    Value::Opaque(Arc::new(QuantityValue(quantity)))
}

fn quantity_ref(value: &Value) -> Option<Quantity> {
    match value {
        Value::Opaque(value) => value.downcast_ref::<QuantityValue>().map(|value| value.0),
        _ => None,
    }
}

fn invalid_receiver(ftx: &FunctionContext, operation: &str) -> ExecutionError {
    ftx.error(format!("{operation}() requires a Kubernetes Quantity"))
}

/// Real upstream's own real definition: `isQuantity(s)` is `true` if and
/// only if `quantity(s)` would not itself error.
pub fn is_quantity(s: &str) -> bool {
    Quantity::parse(s).is_ok()
}

/// The real CEL binding — a free function (`isQuantity('1.5G')`), not a
/// member call, matching real upstream's own real grammar exactly
/// (unlike every function in `kubernetes_lists`, which are all member
/// calls on a list/string receiver).
pub fn is_quantity_binding(s: Arc<String>) -> bool {
    is_quantity(&s)
}

/// Construct the opaque CEL value used by the Kubernetes quantity library.
pub fn quantity_binding(ftx: &FunctionContext, s: Arc<String>) -> Result<Value, ExecutionError> {
    Quantity::parse(&s)
        .map(opaque)
        .map_err(|error| ftx.error(error.to_string()))
}

pub fn is_integer_binding(ftx: &FunctionContext, This(value): This<Value>) -> Result<bool, ExecutionError> {
    quantity_ref(&value).map(|quantity| quantity.as_integer().is_some()).ok_or_else(|| invalid_receiver(ftx, "isInteger"))
}

pub fn as_integer_binding(ftx: &FunctionContext, This(value): This<Value>) -> Result<i64, ExecutionError> {
    let quantity = quantity_ref(&value).ok_or_else(|| invalid_receiver(ftx, "asInteger"))?;
    quantity.as_integer().ok_or_else(|| ftx.error("cannot convert value to integer"))
}

pub fn as_approximate_float_binding(ftx: &FunctionContext, This(value): This<Value>) -> Result<f64, ExecutionError> {
    quantity_ref(&value).map(|quantity| quantity.as_approximate_float()).ok_or_else(|| invalid_receiver(ftx, "asApproximateFloat"))
}

pub fn sign_binding(ftx: &FunctionContext, This(value): This<Value>) -> Result<i64, ExecutionError> {
    quantity_ref(&value).map(|quantity| quantity.sign()).ok_or_else(|| invalid_receiver(ftx, "sign"))
}

fn operand_quantity(ftx: &FunctionContext, value: Value, operation: &str) -> Result<Quantity, ExecutionError> {
    quantity_ref(&value).ok_or_else(|| invalid_receiver(ftx, operation))
}

fn integer_quantity(value: i64) -> Result<Quantity, ExecutionError> {
    Quantity::parse(&value.to_string()).map_err(|error| ExecutionError::function_error("quantity", error.to_string()))
}

pub fn add_binding(ftx: &FunctionContext, This(value): This<Value>, operand: Value) -> Result<Value, ExecutionError> {
    let quantity = quantity_ref(&value).ok_or_else(|| invalid_receiver(ftx, "add"))?;
    let result = match operand {
        Value::Int(value) => quantity + integer_quantity(value)?,
        Value::Opaque(_) => quantity + operand_quantity(ftx, operand, "add")?,
        _ => return Err(ftx.error("add() requires a Quantity or integer operand")),
    };
    Ok(opaque(result))
}

pub fn sub_binding(ftx: &FunctionContext, This(value): This<Value>, operand: Value) -> Result<Value, ExecutionError> {
    let quantity = quantity_ref(&value).ok_or_else(|| invalid_receiver(ftx, "sub"))?;
    let result = match operand {
        Value::Int(value) => quantity + integer_quantity(value)?.negated(),
        Value::Opaque(_) => quantity + operand_quantity(ftx, operand, "sub")?.negated(),
        _ => return Err(ftx.error("sub() requires a Quantity or integer operand")),
    };
    Ok(opaque(result))
}

pub fn is_less_than_binding(ftx: &FunctionContext, This(value): This<Value>, operand: Value) -> Result<bool, ExecutionError> {
    let quantity = quantity_ref(&value).ok_or_else(|| invalid_receiver(ftx, "isLessThan"))?;
    Ok(quantity < operand_quantity(ftx, operand, "isLessThan")?)
}

pub fn is_greater_than_binding(ftx: &FunctionContext, This(value): This<Value>, operand: Value) -> Result<bool, ExecutionError> {
    let quantity = quantity_ref(&value).ok_or_else(|| invalid_receiver(ftx, "isGreaterThan"))?;
    Ok(quantity > operand_quantity(ftx, operand, "isGreaterThan")?)
}

pub fn compare_to_binding(ftx: &FunctionContext, This(value): This<Value>, operand: Value) -> Result<i64, ExecutionError> {
    let quantity = quantity_ref(&value).ok_or_else(|| invalid_receiver(ftx, "compareTo"))?;
    Ok(match quantity.cmp(&operand_quantity(ftx, operand, "compareTo")?) {
        std::cmp::Ordering::Less => -1,
        std::cmp::Ordering::Equal => 0,
        std::cmp::Ordering::Greater => 1,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_real_decimal_si_quantity_is_valid() {
        assert!(is_quantity("1.5G"));
        assert!(is_quantity("200k"));
    }

    #[test]
    fn a_real_binary_si_quantity_is_valid() {
        assert!(is_quantity("1.3Gi"));
        assert!(is_quantity("50Mi"));
    }

    #[test]
    fn an_unrecognized_suffix_is_not_a_valid_quantity() {
        assert!(!is_quantity("5Xi"));
    }

    #[test]
    fn a_non_numeric_string_is_not_a_valid_quantity() {
        assert!(!is_quantity("Three"));
        assert!(!is_quantity("Mi"));
    }

    #[test]
    fn an_empty_string_is_not_a_valid_quantity() {
        assert!(!is_quantity(""));
    }
}
