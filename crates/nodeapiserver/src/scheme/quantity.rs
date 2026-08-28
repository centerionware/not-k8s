//! `Quantity` — a parser for real upstream's own resource-quantity string
//! format (`staging/src/k8s.io/apimachinery/pkg/api/resource/quantity.go`,
//! release-1.34, fetched and read directly — its own grammar comment is
//! quoted below verbatim), the format every `resources.requests`/
//! `resources.limits`/`spec.resources.requests` value (CPU millicores,
//! memory bytes, storage bytes, …) is written in:
//!
//! ```text
//! <quantity>        ::= <signedNumber><suffix>
//! <number>          ::= <digits> | <digits>.<digits> | <digits>. | .<digits>
//! <sign>            ::= "+" | "-"
//! <signedNumber>    ::= <number> | <sign><number>
//! <suffix>          ::= <binarySI> | <decimalExponent> | <decimalSI>
//! <binarySI>        ::= Ki | Mi | Gi | Ti | Pi | Ei
//! <decimalSI>       ::= m | "" | k | M | G | T | P | E
//! <decimalExponent> ::= "e" <signedNumber> | "E" <signedNumber>
//! ```
//!
//! **Named honestly, not a byte-for-byte port**: real upstream's
//! `Quantity` falls back to an arbitrary-precision decimal (`inf.Dec`)
//! whenever a value would overflow `int64`, so it can represent literally
//! any magnitude exactly. This module doesn't carry that fallback — every
//! parsed value is held as an exact `i128` count of milli-units
//! (thousandths), which is lossless for every magnitude any real
//! Kubernetes resource request/limit/quota has ever practically used (an
//! `i128` milli-unit count overflows only past roughly `1.7 * 10^35`
//! whole units — for scale, the entire observable universe's mass in
//! grams is around `10^57`, so this ceiling is not a realistic concern for
//! CPU/memory/storage quantities). A quantity whose milli-value would
//! itself overflow `i128` returns [`Error::Overflow`] rather than falling
//! back to arbitrary precision.
//!
//! [`Quantity::value`]/[`Quantity::milli_value`] mirror real upstream's own
//! `Value()`/`MilliValue()` — both round up (`ceil`), per upstream's own
//! documented behavior ("0.1m will be rounded up to 1m"). Ordering
//! (`Ord`/`PartialOrd`) compares the exact internal milli-unit `i128`
//! directly — this is *more* precise than upstream's own comparison path
//! for values near `int64`'s limits (which downcasts to `int64`/`float64`
//! along the way), so real upstream's `MaxMilliValue`
//! overflow-avoidance dance (used by
//! `plugin/pkg/admission/limitranger`'s own `requestLimitEnforcedValues`)
//! has no equivalent here — there is no overflow to avoid within any
//! realistic magnitude.

use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Quantity {
    /// The exact value, in thousandths of a unit.
    milli: i128,
}

#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    #[error("quantities must not be empty")]
    Empty,
    #[error("{0:?} is not a valid quantity: no digits found")]
    NoDigits(String),
    #[error("{0:?} is not a valid quantity: unrecognized suffix {1:?}")]
    UnrecognizedSuffix(String, String),
    #[error("{0:?} is not a valid quantity: the exponent is not a valid integer")]
    InvalidExponent(String),
    #[error("{0:?} is not a valid quantity: the value is too large to represent")]
    Overflow(String),
}

impl Quantity {
    /// Real upstream's own `Value()` — `ceil` to the nearest whole unit.
    pub fn value(&self) -> i64 {
        ceil_div(self.milli, 1000).clamp(i64::MIN as i128, i64::MAX as i128) as i64
    }

    /// Real upstream's own `MilliValue()` — the exact thousandths, ceiled
    /// (a no-op ceil here, since the internal representation already *is*
    /// milli-units — real upstream's own `MilliValue()` needs to ceil
    /// because its internal scale may be coarser than milli; this port's
    /// internal scale never is).
    pub fn milli_value(&self) -> i64 {
        self.milli.clamp(i64::MIN as i128, i64::MAX as i128) as i64
    }

    pub fn is_zero(&self) -> bool {
        self.milli == 0
    }

    /// The zero quantity — the correct starting accumulator for summing
    /// (`+`) a resource across containers, the way real upstream's own
    /// `addResourceList` does starting from an absent map entry.
    pub const ZERO: Quantity = Quantity { milli: 0 };

    /// Real upstream's own `maxResourceList`'s per-key comparison —
    /// exact, since ordering here is exact `i128` comparison (see this
    /// module's own doc comment).
    pub fn max(self, other: Quantity) -> Quantity {
        if self >= other {
            self
        } else {
            other
        }
    }

    pub fn parse(s: &str) -> Result<Quantity, Error> {
        let s = s.trim();
        if s.is_empty() {
            return Err(Error::Empty);
        }
        let bytes = s.as_bytes();
        let mut idx = 0;
        let negative = bytes[0] == b'-';
        if bytes[0] == b'+' || bytes[0] == b'-' {
            idx = 1;
        }

        let int_start = idx;
        while idx < bytes.len() && bytes[idx].is_ascii_digit() {
            idx += 1;
        }
        let int_part = &s[int_start..idx];

        let mut frac_part = "";
        if idx < bytes.len() && bytes[idx] == b'.' {
            idx += 1;
            let frac_start = idx;
            while idx < bytes.len() && bytes[idx].is_ascii_digit() {
                idx += 1;
            }
            frac_part = &s[frac_start..idx];
        }

        if int_part.is_empty() && frac_part.is_empty() {
            return Err(Error::NoDigits(s.to_string()));
        }

        let suffix = &s[idx..];
        let digits = format!("{int_part}{frac_part}");
        let digits_i128: i128 = if digits.is_empty() { 0 } else { digits.parse().map_err(|_| Error::Overflow(s.to_string()))? };
        let digits_i128 = if negative { -digits_i128 } else { digits_i128 };
        let frac_len = frac_part.len() as i32;

        let (decimal_exponent, binary_pow): (i32, u32) = match suffix {
            "" => (0, 0),
            "m" => (-3, 0),
            "k" => (3, 0),
            "M" => (6, 0),
            "G" => (9, 0),
            "T" => (12, 0),
            "P" => (15, 0),
            "E" => (18, 0),
            "Ki" => (0, 10),
            "Mi" => (0, 20),
            "Gi" => (0, 30),
            "Ti" => (0, 40),
            "Pi" => (0, 50),
            "Ei" => (0, 60),
            _ if suffix.starts_with('e') || suffix.starts_with('E') => {
                let exp: i32 = suffix[1..].parse().map_err(|_| Error::InvalidExponent(s.to_string()))?;
                (exp, 0)
            }
            _ => return Err(Error::UnrecognizedSuffix(s.to_string(), suffix.to_string())),
        };

        let binary_multiplier: i128 = 2i128.checked_pow(binary_pow).ok_or_else(|| Error::Overflow(s.to_string()))?;
        // +3 for the milli scale every internal value is held at.
        let total_pow10 = decimal_exponent - frac_len + 3;

        let milli = if total_pow10 >= 0 {
            let pow = 10i128.checked_pow(total_pow10 as u32).ok_or_else(|| Error::Overflow(s.to_string()))?;
            digits_i128.checked_mul(pow).and_then(|v| v.checked_mul(binary_multiplier)).ok_or_else(|| Error::Overflow(s.to_string()))?
        } else {
            let pow = 10i128.checked_pow((-total_pow10) as u32).ok_or_else(|| Error::Overflow(s.to_string()))?;
            let numerator = digits_i128.checked_mul(binary_multiplier).ok_or_else(|| Error::Overflow(s.to_string()))?;
            ceil_div(numerator, pow)
        };

        Ok(Quantity { milli })
    }
}

/// Real upstream's own `addResourceList`'s per-key addition — exact,
/// since the internal representation is already exact `i128` milli-units
/// (see this module's own doc comment); saturates rather than wrapping
/// on the (unrealistic) overflow case, same posture as
/// [`Quantity::value`]/[`Quantity::milli_value`]'s own clamp.
impl std::ops::Add for Quantity {
    type Output = Quantity;
    fn add(self, rhs: Quantity) -> Quantity {
        Quantity { milli: self.milli.saturating_add(rhs.milli) }
    }
}

impl fmt::Display for Quantity {
    /// Not upstream's own canonical serialization (which picks the
    /// largest suffix that loses no precision) — a plain decimal
    /// rendering of the exact milli-unit value, good enough for the
    /// human-readable admission-denial messages this crate's callers use
    /// it for, named honestly rather than claimed as round-trip-faithful.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let whole = self.milli / 1000;
        let frac = (self.milli % 1000).abs();
        if frac == 0 {
            write!(f, "{whole}")
        } else {
            write!(f, "{whole}.{frac:03}")
        }
    }
}

fn ceil_div(numerator: i128, denominator: i128) -> i128 {
    let q = numerator / denominator;
    let r = numerator % denominator;
    if r != 0 && numerator > 0 {
        q + 1
    } else {
        q
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_plain_integer() {
        assert_eq!(Quantity::parse("5").unwrap().value(), 5);
    }

    #[test]
    fn parses_a_decimal() {
        assert_eq!(Quantity::parse("1.5").unwrap().milli_value(), 1500);
    }

    #[test]
    fn parses_a_leading_dot_decimal() {
        assert_eq!(Quantity::parse(".5").unwrap().milli_value(), 500);
    }

    #[test]
    fn parses_a_negative_value() {
        assert_eq!(Quantity::parse("-5").unwrap().value(), -5);
    }

    #[test]
    fn parses_the_milli_suffix() {
        assert_eq!(Quantity::parse("100m").unwrap().milli_value(), 100);
        assert_eq!(Quantity::parse("1500m").unwrap().value(), 2, "0.5 rounds up per upstream's own ceil semantics");
    }

    #[test]
    fn parses_decimal_si_suffixes() {
        assert_eq!(Quantity::parse("1k").unwrap().value(), 1_000);
        assert_eq!(Quantity::parse("1M").unwrap().value(), 1_000_000);
        assert_eq!(Quantity::parse("1G").unwrap().value(), 1_000_000_000);
    }

    #[test]
    fn parses_binary_si_suffixes() {
        assert_eq!(Quantity::parse("1Ki").unwrap().value(), 1_024);
        assert_eq!(Quantity::parse("1Mi").unwrap().value(), 1_048_576);
        assert_eq!(Quantity::parse("1Gi").unwrap().value(), 1_073_741_824);
    }

    #[test]
    fn parses_a_fractional_binary_value_exactly() {
        assert_eq!(Quantity::parse("1.5Gi").unwrap().value(), 1_610_612_736);
    }

    #[test]
    fn parses_decimal_exponent_suffixes() {
        assert_eq!(Quantity::parse("1e3").unwrap().value(), 1_000);
        assert_eq!(Quantity::parse("1E3").unwrap().value(), 1_000);
        assert_eq!(Quantity::parse("1e-3").unwrap().milli_value(), 1);
    }

    #[test]
    fn the_bare_e_suffix_means_exa_not_an_exponent() {
        // "E" alone is decimalSI exa (10^18); only "e"/"E" *followed by* a
        // signed number is the decimalExponent form.
        assert_eq!(Quantity::parse("1E").unwrap().value(), 1_000_000_000_000_000_000);
    }

    #[test]
    fn rounds_up_a_sub_milli_fraction_per_upstreams_own_documented_example() {
        // Real upstream's own doc comment: "0.1m will be rounded up to 1m".
        assert_eq!(Quantity::parse("0.1m").unwrap().milli_value(), 1);
    }

    #[test]
    fn an_empty_string_is_rejected() {
        assert_eq!(Quantity::parse(""), Err(Error::Empty));
    }

    #[test]
    fn a_string_with_no_digits_is_rejected() {
        assert!(matches!(Quantity::parse("Gi"), Err(Error::NoDigits(_))));
    }

    #[test]
    fn an_unrecognized_suffix_is_rejected() {
        assert!(matches!(Quantity::parse("5Xi"), Err(Error::UnrecognizedSuffix(_, _))));
    }

    #[test]
    fn ordering_compares_by_real_value_not_string() {
        assert!(Quantity::parse("1Gi").unwrap() > Quantity::parse("1G").unwrap());
        assert!(Quantity::parse("500m").unwrap() < Quantity::parse("1").unwrap());
        assert_eq!(Quantity::parse("1000m").unwrap(), Quantity::parse("1").unwrap());
    }

    #[test]
    fn display_renders_a_plain_decimal() {
        assert_eq!(Quantity::parse("1.5").unwrap().to_string(), "1.500");
        assert_eq!(Quantity::parse("5").unwrap().to_string(), "5");
    }

    #[test]
    fn add_sums_two_quantities_exactly() {
        let sum = Quantity::parse("100m").unwrap() + Quantity::parse("1").unwrap();
        assert_eq!(sum.milli_value(), 1100);
    }

    #[test]
    fn zero_is_the_correct_additive_identity() {
        assert_eq!((Quantity::ZERO + Quantity::parse("500m").unwrap()).milli_value(), 500);
    }

    #[test]
    fn max_picks_the_larger_quantity() {
        let a = Quantity::parse("1Gi").unwrap();
        let b = Quantity::parse("500Mi").unwrap();
        assert_eq!(a.max(b), a);
        assert_eq!(b.max(a), a);
    }
}
