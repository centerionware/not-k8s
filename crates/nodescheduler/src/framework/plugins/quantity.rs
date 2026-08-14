//! The `quantity(...)` CEL function and its methods — upstream's
//! `k8s.io/apiserver/pkg/cel/library`'s `Quantity` library, ported onto
//! `cel-interpreter`'s `Value::Float` rather than a genuine opaque
//! `Quantity` type. See `dynamic_resources.rs`'s module header ("The CEL
//! environment this exposes, and where it diverges from upstream") for why:
//! `cel-interpreter`'s `Value` is a closed enum with no custom-type variant,
//! so there is no way to add a real `Quantity` without forking the crate.
//!
//! Registered into every `Context` `device_matches` builds, alongside the
//! `device` variable — see `install`.
//!
//! Function names and signatures match upstream's real doc comment
//! (`staging/src/k8s.io/apiserver/pkg/cel/library/quantity.go`) exactly:
//! `quantity(<string>)`, `isQuantity(<string>)`, and
//! `<quantity>.{isInteger,asInteger,asApproximateFloat,sign,add,sub,
//! isGreaterThan,isLessThan,compareTo}(...)`. Arithmetic/equality operators
//! (`==`, `<`, `>`) are not registered here at all — `cel-interpreter`
//! already implements those natively for `Value::Float`, and a `Quantity`
//! value under this scheme *is* a `Value::Float`, so they already work.

use cel_interpreter::extractors::This;
use cel_interpreter::{Context, ExecutionError, FunctionContext, Value};

type Result<T> = std::result::Result<T, ExecutionError>;

/// Registers the whole library into `ctx`. Idempotent to call more than
/// once (each call just overwrites the same names), but `device_matches`
/// only ever builds one `Context` per selector evaluation, so it doesn't.
pub fn install(ctx: &mut Context) {
    ctx.add_function("quantity", quantity);
    ctx.add_function("isQuantity", is_quantity);
    ctx.add_function("isInteger", is_integer);
    ctx.add_function("asInteger", as_integer);
    ctx.add_function("asApproximateFloat", as_approximate_float);
    ctx.add_function("sign", sign);
    ctx.add_function("add", add);
    ctx.add_function("sub", sub);
    ctx.add_function("isGreaterThan", is_greater_than);
    ctx.add_function("isLessThan", is_less_than);
    ctx.add_function("compareTo", compare_to);
}

/// A quantity string's numeric value, in base units — the same parser
/// `cache/pod.rs` uses for CPU/memory quantities, reused here so
/// `quantity("1Gi")` and a `ResourceSlice`'s own `capacity` entries agree on
/// what a suffix means.
fn parse(s: &str) -> Option<f64> {
    crate::cache::pod::parse_quantity_f64(s)
}

/// `quantity(<string>) <Quantity>` — errors (does not merely return some
/// sentinel) on a string that isn't a valid quantity, matching upstream: a
/// selector calling this on bad input should fail closed via
/// `device_matches`'s own "doesn't evaluate to a plain `true`" rule, not
/// silently compare against zero.
fn quantity(ftx: &FunctionContext, This(this): This<std::sync::Arc<String>>) -> Result<Value> {
    parse(&this).map(Value::Float).ok_or_else(|| ftx.error(format!("invalid quantity {:?}", this.as_str())))
}

/// `isQuantity(<string>) <bool>`
fn is_quantity(This(this): This<std::sync::Arc<String>>) -> bool {
    parse(&this).is_some()
}

fn as_float(v: &Value) -> Option<f64> {
    match v {
        Value::Float(f) => Some(*f),
        Value::Int(i) => Some(*i as f64),
        Value::UInt(u) => Some(*u as f64),
        _ => None,
    }
}

/// `<Quantity>.isInteger() <bool>`
fn is_integer(ftx: &FunctionContext, This(this): This<Value>) -> Result<bool> {
    let f = as_float(&this).ok_or_else(|| ftx.error(format!("{this:?} is not a quantity")))?;
    Ok(f.fract() == 0.0 && f.is_finite() && f >= i64::MIN as f64 && f <= i64::MAX as f64)
}

/// `<Quantity>.asInteger() <int>`
fn as_integer(ftx: &FunctionContext, This(this): This<Value>) -> Result<i64> {
    let f = as_float(&this).ok_or_else(|| ftx.error(format!("{this:?} is not a quantity")))?;
    if f.fract() != 0.0 || !f.is_finite() || f < i64::MIN as f64 || f > i64::MAX as f64 {
        return Err(ftx.error("cannot convert value to integer"));
    }
    Ok(f as i64)
}

/// `<Quantity>.asApproximateFloat() <float>`
fn as_approximate_float(ftx: &FunctionContext, This(this): This<Value>) -> Result<f64> {
    as_float(&this).ok_or_else(|| ftx.error(format!("{this:?} is not a quantity")))
}

/// `<Quantity>.sign() <int>`
fn sign(ftx: &FunctionContext, This(this): This<Value>) -> Result<i64> {
    let f = as_float(&this).ok_or_else(|| ftx.error(format!("{this:?} is not a quantity")))?;
    Ok(if f > 0.0 {
        1
    } else if f < 0.0 {
        -1
    } else {
        0
    })
}

/// `<Quantity>.add(<quantity>|<integer>) <quantity>`
fn add(ftx: &FunctionContext, This(this): This<Value>, other: Value) -> Result<Value> {
    let a = as_float(&this).ok_or_else(|| ftx.error(format!("{this:?} is not a quantity")))?;
    let b = as_float(&other).ok_or_else(|| ftx.error(format!("{other:?} is not a quantity or integer")))?;
    Ok(Value::Float(a + b))
}

/// `<Quantity>.sub(<quantity>|<integer>) <quantity>`
fn sub(ftx: &FunctionContext, This(this): This<Value>, other: Value) -> Result<Value> {
    let a = as_float(&this).ok_or_else(|| ftx.error(format!("{this:?} is not a quantity")))?;
    let b = as_float(&other).ok_or_else(|| ftx.error(format!("{other:?} is not a quantity or integer")))?;
    Ok(Value::Float(a - b))
}

/// `<Quantity>.isGreaterThan(<quantity>) <bool>`
fn is_greater_than(ftx: &FunctionContext, This(this): This<Value>, other: Value) -> Result<bool> {
    let a = as_float(&this).ok_or_else(|| ftx.error(format!("{this:?} is not a quantity")))?;
    let b = as_float(&other).ok_or_else(|| ftx.error(format!("{other:?} is not a quantity")))?;
    Ok(a > b)
}

/// `<Quantity>.isLessThan(<quantity>) <bool>`
fn is_less_than(ftx: &FunctionContext, This(this): This<Value>, other: Value) -> Result<bool> {
    let a = as_float(&this).ok_or_else(|| ftx.error(format!("{this:?} is not a quantity")))?;
    let b = as_float(&other).ok_or_else(|| ftx.error(format!("{other:?} is not a quantity")))?;
    Ok(a < b)
}

/// `<Quantity>.compareTo(<quantity>) <int>`
fn compare_to(ftx: &FunctionContext, This(this): This<Value>, other: Value) -> Result<i64> {
    let a = as_float(&this).ok_or_else(|| ftx.error(format!("{this:?} is not a quantity")))?;
    let b = as_float(&other).ok_or_else(|| ftx.error(format!("{other:?} is not a quantity")))?;
    Ok(if a > b {
        1
    } else if a < b {
        -1
    } else {
        0
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn eval(expr: &str) -> Value {
        let mut ctx = Context::default();
        install(&mut ctx);
        let program = cel_interpreter::Program::compile(expr).unwrap();
        program.execute(&ctx).unwrap()
    }

    #[test]
    fn quantity_parses_a_valid_string() {
        assert_eq!(eval(r#"quantity("1.5G")"#), Value::Float(1_500_000_000.0));
    }

    #[test]
    fn quantity_on_an_invalid_string_is_an_error_not_a_silent_zero() {
        let mut ctx = Context::default();
        install(&mut ctx);
        let program = cel_interpreter::Program::compile(r#"quantity("not a quantity")"#).unwrap();
        assert!(program.execute(&ctx).is_err());
    }

    #[test]
    fn is_quantity_distinguishes_valid_from_invalid() {
        assert_eq!(eval(r#"isQuantity("1.3Gi")"#), Value::Bool(true));
        assert_eq!(eval(r#"isQuantity("Three")"#), Value::Bool(false));
    }

    #[test]
    fn comparisons_match_upstreams_documented_examples() {
        assert_eq!(eval(r#"quantity("200M").compareTo(quantity("0.2G"))"#), Value::Int(0));
        assert_eq!(eval(r#"quantity("50M").compareTo(quantity("50Mi"))"#), Value::Int(-1));
        assert_eq!(eval(r#"quantity("150Mi").isGreaterThan(quantity("100Mi"))"#), Value::Bool(true));
        assert_eq!(eval(r#"quantity("50M").isLessThan(quantity("100M"))"#), Value::Bool(true));
    }

    #[test]
    fn arithmetic_matches_upstreams_documented_examples() {
        assert_eq!(eval(r#"quantity("50k").add(20).sub(quantity("100k")).sub(-50000) == quantity("20")"#), Value::Bool(true));
    }

    #[test]
    fn scalar_conversions_match_upstreams_documented_examples() {
        assert_eq!(eval(r#"quantity("50000000G").isInteger()"#), Value::Bool(true));
        assert_eq!(eval(r#"quantity("50k").asInteger() == 50000"#), Value::Bool(true));
    }

    #[test]
    fn native_equality_and_ordering_already_work_on_the_underlying_float() {
        // No registration needed for these — see the module header.
        assert_eq!(eval(r#"quantity("1Gi") > quantity("1G")"#), Value::Bool(true));
        assert_eq!(eval(r#"quantity("1G") == quantity("1000000000")"#), Value::Bool(true));
    }
}
