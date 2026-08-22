//! Real upstream's own `kubernetes.lists` CEL extension library
//! (`k8s.io/apiserver/pkg/cel/library/lists.go`, fetched and read
//! directly) — this crate's first entry in `docs/APISERVER.md`'s own
//! Group K point 6 ("Kubernetes' own CEL extension library ... `isSorted`
//! ..."), named there as deliberately deferred until a first working CEL
//! path existed. Scoped to `isSorted()` only for this landing: real
//! upstream's own library also has `sum`/`min`/`max`/`indexOf`/
//! `lastIndexOf`/`includes` — separate, not-yet-started work, named
//! honestly rather than silently folded in as "done".
//!
//! [`is_sorted`] is the pure, directly-testable core; [`is_sorted_binding`]
//! is the thin adapter `cel_ext::register_kubernetes_extensions` registers
//! onto a real [`cel::Context`] via `Context::add_function` — the `This`
//! extractor (`cel::extractors::This`, confirmed against `cel-rust`'s own
//! `example/src/functions.rs`, not assumed from the published docs alone,
//! which don't render this module) is cel-rust's own real convention for
//! a member-call's receiver (`<list>.isSorted()` calls `isSorted` with the
//! list itself as `This`).

use cel::extractors::This;
use cel::Value;
use std::sync::Arc;

/// Real upstream's own real semantics, doc-comment examples confirmed
/// directly: `[].isSorted()` → `true`, `[1].isSorted()` → `true`, equal
/// adjacent elements still count as sorted (`['a','b','b','c'].isSorted()`
/// → `true`). **A real, honest divergence from real upstream**: real
/// upstream's own CEL overload declarations restrict this function at
/// *compile time* to a single comparable element type per call
/// (`list<T>.isSorted()`, `T` fixed); this crate's own binding has no
/// such compile-time restriction (this crate's CEL layer has no type
/// checker at all — `docs/APISERVER.md`'s own `cel_ext` section names
/// this), so a list mixing genuinely incomparable element types (e.g.
/// an int next to a string) is treated as "not sorted" here (`Value::
/// partial_cmp` returns `None` for an incomparable pair) rather than
/// real upstream's own compile-time rejection.
pub fn is_sorted(list: &[Value]) -> bool {
    list.windows(2).all(|pair| pair[0].partial_cmp(&pair[1]).is_some_and(|o| o != std::cmp::Ordering::Greater))
}

/// The real CEL binding — see this module's own doc comment for the
/// `This` extractor's real meaning.
pub fn is_sorted_binding(This(list): This<Arc<Vec<Value>>>) -> bool {
    is_sorted(&list)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_list_is_sorted() {
        assert!(is_sorted(&[]));
    }

    #[test]
    fn a_single_element_list_is_sorted() {
        assert!(is_sorted(&[Value::Int(1)]));
    }

    #[test]
    fn ascending_integers_are_sorted() {
        assert!(is_sorted(&[Value::Int(1), Value::Int(2), Value::Int(3)]));
    }

    #[test]
    fn descending_integers_are_not_sorted() {
        assert!(!is_sorted(&[Value::Int(2), Value::Int(1)]));
    }

    #[test]
    fn equal_adjacent_elements_still_count_as_sorted() {
        assert!(is_sorted(&[Value::Int(1), Value::Int(1), Value::Int(2)]));
    }

    #[test]
    fn ascending_strings_are_sorted() {
        let list = vec![
            Value::String(Arc::new("a".to_string())),
            Value::String(Arc::new("b".to_string())),
            Value::String(Arc::new("b".to_string())),
            Value::String(Arc::new("c".to_string())),
        ];
        assert!(is_sorted(&list));
    }

    #[test]
    fn descending_floats_are_not_sorted() {
        assert!(!is_sorted(&[Value::Float(2.0), Value::Float(1.0)]));
    }

    #[test]
    fn an_incomparable_pair_counts_as_not_sorted_rather_than_erroring() {
        assert!(!is_sorted(&[Value::Int(1), Value::String(Arc::new("x".to_string()))]));
    }
}
