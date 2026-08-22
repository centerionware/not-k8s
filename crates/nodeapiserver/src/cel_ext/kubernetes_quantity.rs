//! Real upstream's own `kubernetes.quantity` CEL extension library
//! (`k8s.io/apiserver/pkg/cel/library/quantity.go`, fetched and read
//! directly) — `docs/APISERVER.md`'s own Group K point 6. Scoped to
//! `isQuantity(<string>)` only for this landing: real upstream's own
//! library also has a real `quantity(<string>) <Quantity>` constructor
//! plus a whole opaque `Quantity` CEL type with its own member functions
//! (`isInteger`/`asInteger`/`asApproximateFloat`/`sign`/`add`/`sub`/
//! `isLessThan`/`isGreaterThan`/`compareTo`) — real, separate,
//! not-yet-started work: registering a genuine opaque CEL value (`cel::
//! Value::Opaque`, `cel::objects::Opaque` trait) is a bigger, riskier
//! lift than this crate's own already-landed `kubernetes_lists` module
//! needed, and deserves its own dedicated session rather than a rushed
//! first attempt bundled in here.
//!
//! [`is_quantity`] reuses this crate's own already-landed
//! [`crate::scheme::quantity::Quantity::parse`] (Group G's own quantity
//! port, already the real parser `admission::limit_ranger`'s min/max/
//! ratio comparisons are built on) rather than re-implementing quantity
//! parsing a second time — `isQuantity` is real upstream's own "does
//! `quantity` not error" definition exactly, so delegating to the same
//! `Result` this crate's own parser already produces is both correct and
//! avoids a second, potentially-diverging implementation.

use crate::scheme::quantity::Quantity;
use std::sync::Arc;

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
