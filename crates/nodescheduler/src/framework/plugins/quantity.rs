//! Kubernetes' CEL Quantity library with exact arithmetic.
//!
//! `cel` has no custom/opaque Value variant, so an exact quantity
//! is carried as a private canonical string (`numerator/denominator`) and all
//! Quantity methods operate on that representation. Equivalent spellings
//! canonicalize identically, while arithmetic never passes through f64.

use cel::extractors::This;
use cel::{Context, ExecutionError, FunctionContext, Value};
use num_bigint::BigInt;
use num_rational::BigRational;
use num_traits::{Signed, ToPrimitive, Zero};
use std::sync::Arc;

type Result<T> = std::result::Result<T, ExecutionError>;
const PREFIX: &str = "__notk8s_quantity:";
/// Keep malformed or adversarial quantities from forcing an unbounded BigInt
/// allocation while still accepting every practical Kubernetes quantity.
const MAX_EXPONENT_MAGNITUDE: u32 = 4096;

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

fn pow10(power: usize) -> BigInt {
    BigInt::from(10u8).pow(power as u32)
}

/// Parse Kubernetes' quantity grammar into exact base units.
fn parse(raw: &str) -> Option<BigRational> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }

    let (number, multiplier) = ["Ki", "Mi", "Gi", "Ti", "Pi", "Ei"]
        .iter()
        .enumerate()
        .find_map(|(index, suffix)| {
            raw.strip_suffix(suffix).map(|number| {
                (
                    number,
                    BigRational::from_integer(BigInt::from(1024u16).pow((index + 1) as u32)),
                )
            })
        })
        .or_else(|| {
            let (suffix, power): (&str, i32) = [
                ("n", -9), ("u", -6), ("m", -3), ("k", 3), ("M", 6),
                ("G", 9), ("T", 12), ("P", 15), ("E", 18),
            ]
            .into_iter()
            .find(|(suffix, _)| raw.ends_with(suffix))?;
            let number = raw.strip_suffix(suffix)?;
            let factor = if power >= 0 {
                BigRational::from_integer(pow10(power as usize))
            } else {
                BigRational::new(BigInt::from(1), pow10((-power) as usize))
            };
            Some((number, factor))
        })
        .unwrap_or((raw, BigRational::from_integer(BigInt::from(1))));

    let (mantissa, exponent) = split_exponent(number)?;
    if exponent.unsigned_abs() > MAX_EXPONENT_MAGNITUDE {
        return None;
    }
    let negative = mantissa.starts_with('-');
    let unsigned = mantissa
        .strip_prefix('-')
        .or_else(|| mantissa.strip_prefix('+'))
        .unwrap_or(mantissa);
    let (whole, fraction) = unsigned.split_once('.').unwrap_or((unsigned, ""));
    if (whole.is_empty() && fraction.is_empty())
        || !whole.chars().all(|c| c.is_ascii_digit())
        || !fraction.chars().all(|c| c.is_ascii_digit())
    {
        return None;
    }
    let mut numerator = format!("{whole}{fraction}").parse::<BigInt>().ok()?;
    if negative {
        numerator = -numerator;
    }
    let mut value = BigRational::new(numerator, pow10(fraction.len()));
    if exponent >= 0 {
        value *= BigRational::from_integer(pow10(exponent as usize));
    } else {
        value /= BigRational::from_integer(pow10((-exponent) as usize));
    }
    Some(value * multiplier)
}

fn split_exponent(number: &str) -> Option<(&str, i32)> {
    for (index, ch) in number.char_indices().rev() {
        if ch != 'e' && ch != 'E' {
            continue;
        }
        let tail = &number[index + 1..];
        let digits = tail
            .strip_prefix('-')
            .or_else(|| tail.strip_prefix('+'))
            .unwrap_or(tail);
        if !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit()) {
            return Some((&number[..index], tail.parse().ok()?));
        }
    }
    Some((number, 0))
}

fn encode(value: &BigRational) -> String {
    format!("{PREFIX}{}/{}", value.numer(), value.denom())
}

fn decode_string(value: &str) -> Option<BigRational> {
    let (numerator, denominator) = value.strip_prefix(PREFIX)?.split_once('/')?;
    let denominator = denominator.parse::<BigInt>().ok()?;
    if denominator.is_zero() {
        return None;
    }
    Some(BigRational::new(numerator.parse().ok()?, denominator))
}

fn decode(value: &Value) -> Option<BigRational> {
    match value {
        Value::String(s) => decode_string(s),
        Value::Int(i) => Some(BigRational::from_integer(BigInt::from(*i))),
        Value::UInt(i) => Some(BigRational::from_integer(BigInt::from(*i))),
        _ => None,
    }
}

pub(crate) fn canonical(raw: &str) -> Option<String> {
    parse(raw).map(|value| encode(&value))
}

fn quantity(ftx: &FunctionContext, This(this): This<Arc<String>>) -> Result<Value> {
    canonical(&this)
        .map(|s| Value::String(Arc::new(s)))
        .ok_or_else(|| ftx.error(format!("invalid quantity {:?}", this.as_str())))
}

fn is_quantity(This(this): This<Arc<String>>) -> bool {
    parse(&this).is_some()
}

fn exact(ftx: &FunctionContext, value: &Value) -> Result<BigRational> {
    decode(value).ok_or_else(|| ftx.error(format!("{value:?} is not a quantity")))
}

fn is_integer(ftx: &FunctionContext, This(this): This<Value>) -> Result<bool> {
    let value = exact(ftx, &this)?;
    Ok(value.is_integer() && value.to_integer().to_i64().is_some())
}

fn as_integer(ftx: &FunctionContext, This(this): This<Value>) -> Result<i64> {
    let value = exact(ftx, &this)?;
    if !value.is_integer() {
        return Err(ftx.error("cannot convert value to integer"));
    }
    value.to_integer().to_i64().ok_or_else(|| ftx.error("cannot convert value to integer"))
}

fn as_approximate_float(ftx: &FunctionContext, This(this): This<Value>) -> Result<f64> {
    let value = exact(ftx, &this)?;
    Ok(value.to_f64().unwrap_or_else(|| if value.is_negative() { f64::NEG_INFINITY } else { f64::INFINITY }))
}

fn sign(ftx: &FunctionContext, This(this): This<Value>) -> Result<i64> {
    let value = exact(ftx, &this)?;
    Ok(if value.is_positive() { 1 } else if value.is_negative() { -1 } else { 0 })
}

fn add(ftx: &FunctionContext, This(this): This<Value>, other: Value) -> Result<Value> {
    Ok(Value::String(Arc::new(encode(&(exact(ftx, &this)? + exact(ftx, &other)?)))))
}

fn sub(ftx: &FunctionContext, This(this): This<Value>, other: Value) -> Result<Value> {
    Ok(Value::String(Arc::new(encode(&(exact(ftx, &this)? - exact(ftx, &other)?)))))
}

fn is_greater_than(ftx: &FunctionContext, This(this): This<Value>, other: Value) -> Result<bool> {
    Ok(exact(ftx, &this)? > exact(ftx, &other)?)
}

fn is_less_than(ftx: &FunctionContext, This(this): This<Value>, other: Value) -> Result<bool> {
    Ok(exact(ftx, &this)? < exact(ftx, &other)?)
}

fn compare_to(ftx: &FunctionContext, This(this): This<Value>, other: Value) -> Result<i64> {
    Ok(match exact(ftx, &this)?.cmp(&exact(ftx, &other)?) {
        std::cmp::Ordering::Greater => 1,
        std::cmp::Ordering::Less => -1,
        std::cmp::Ordering::Equal => 0,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn eval(expr: &str) -> Value {
        let mut ctx = Context::default();
        install(&mut ctx);
        cel::Program::compile(expr).unwrap().execute(&ctx).unwrap()
    }

    #[test]
    fn quantities_are_exact_beyond_float64s_precision() {
        assert_eq!(eval(r#"quantity("9007199254740993").compareTo(quantity("9007199254740992"))"#), Value::Int(1));
    }

    #[test]
    fn equivalent_spellings_have_the_same_canonical_value() {
        assert_eq!(eval(r#"quantity("1G") == quantity("1000000000")"#), Value::Bool(true));
        assert_eq!(eval(r#"quantity("200M").compareTo(quantity("0.2G"))"#), Value::Int(0));
    }

    #[test]
    fn binary_decimal_and_exponent_suffixes_are_exact() {
        assert_eq!(eval(r#"quantity("1Gi").asInteger()"#), Value::Int(1_073_741_824));
        assert_eq!(eval(r#"quantity("1e3").asInteger()"#), Value::Int(1000));
        assert_eq!(eval(r#"quantity("1m").isInteger()"#), Value::Bool(false));
    }

    #[test]
    fn exponent_magnitude_is_finite_and_handles_i32_min() {
        assert!(parse("1e4096").is_some());
        assert!(parse("1e4097").is_none());
        assert!(parse("1e-4097").is_none());
        assert!(parse("1e-2147483648").is_none());
    }

    #[test]
    fn arithmetic_and_scalar_conversions_match_upstream() {
        assert_eq!(eval(r#"quantity("50k").add(20).sub(quantity("100k")).sub(-50000).asInteger()"#), Value::Int(20));
    }

    #[test]
    fn invalid_quantities_fail_closed() {
        assert_eq!(eval(r#"isQuantity("Three")"#), Value::Bool(false));
        let mut ctx = Context::default();
        install(&mut ctx);
        let program = cel::Program::compile(r#"quantity("Three")"#).unwrap();
        assert!(program.execute(&ctx).is_err());
    }
}
