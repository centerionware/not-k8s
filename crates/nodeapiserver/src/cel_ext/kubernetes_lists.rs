//! Real upstream's own `kubernetes.lists` CEL extension library
//! (`k8s.io/apiserver/pkg/cel/library/lists.go`, fetched and read
//! directly) — `docs/APISERVER.md`'s own Group K point 6 ("Kubernetes'
//! own CEL extension library ... `isSorted` ..."), named there as
//! deliberately deferred until a first working CEL path existed.
//! `isSorted`/`min`/`max`/`indexOf`/`lastIndexOf`/`sum` are landed;
//! `includes` (needs a `This<Value>` receiver that isn't necessarily a
//! list — real upstream's own doc comment: `'model-a'.includes('model-a')`
//! also works on a bare string) is separate, not-yet-started work, named
//! honestly rather than silently folded in as "done".
//!
//! Every function here follows the same shape: a pure, directly-testable
//! core taking `&[Value]` (plus whatever extra argument the real function
//! needs), and a thin `_binding` adapter `cel_ext::
//! register_kubernetes_extensions` registers onto a real [`cel::Context`]
//! via `Context::add_function`. The `This`/`FunctionContext` shapes
//! (`cel::extractors::This`, `cel::FunctionContext`) are confirmed
//! against `cel-rust`'s own real source
//! (`example/src/functions.rs`/`cel/src/functions.rs`'s own
//! `contains`/`string`/`size` — none of this is rendered by the published
//! docs, which only expose the crate's own generated API reference, not
//! its example/internal modules) rather than assumed: `This<Arc<Vec<
//! Value>>>` is the real member-call receiver for a list, `&FunctionContext`
//! as an additional argument gives a real function access to `ftx.error(...)`
//! for a genuine CEL execution error (real upstream's own `min`/`max`
//! erroring on an empty list, ported exactly).

use cel::extractors::This;
use cel::{ExecutionError, FunctionContext, Value};
use std::cmp::Ordering;
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

/// Real upstream's own real scan (`cmp("min", types.IntOne)`): keeps the
/// running minimum, replacing it only on a real "current is greater than
/// next" comparison. **Named, honest divergence, same shape as
/// [`is_sorted`]'s own**: real upstream's own `Compare` returning an
/// error for an incomparable pair silently leaves the running result
/// unchanged (it never actually happens in real usage, since real
/// upstream's own compile-time overloads already fix every element to
/// one comparable type); this crate's own `partial_cmp` returning `None`
/// gets the same "leave it unchanged" treatment for the same reason —
/// not a real error path, just what naturally falls out of "no match
/// means don't replace".
pub fn min(list: &[Value]) -> Result<Value, &'static str> {
    scan(list, Ordering::Greater).ok_or("min() called on an empty list")
}

/// [`min`]'s own mirror — keeps the running maximum, replacing it only on
/// a real "current is less than next" comparison.
pub fn max(list: &[Value]) -> Result<Value, &'static str> {
    scan(list, Ordering::Less).ok_or("max() called on an empty list")
}

/// The shared real scan [`min`]/[`max`] are both built from — `replace_on`
/// is the real `Ordering` (`result.partial_cmp(next)`) that means "next
/// should become the new running result".
fn scan(list: &[Value], replace_on: Ordering) -> Option<Value> {
    let mut result: Option<&Value> = None;
    for item in list {
        result = Some(match result {
            None => item,
            Some(current) => if current.partial_cmp(item) == Some(replace_on) { item } else { current },
        });
    }
    result.cloned()
}

/// Real upstream's own real linear scan (`Equal`, CEL's own equality —
/// this crate's `Value::eq`), returning the first matching index or `-1`
/// when the item isn't found at all.
pub fn index_of(list: &[Value], item: &Value) -> i64 {
    list.iter().position(|v| v == item).map(|i| i as i64).unwrap_or(-1)
}

/// [`index_of`]'s own mirror — the *last* matching index.
pub fn last_index_of(list: &[Value], item: &Value) -> i64 {
    list.iter().rposition(|v| v == item).map(|i| i as i64).unwrap_or(-1)
}

/// The real CEL bindings — `ftx.error(...)` is this crate's own real
/// route to a genuine `ExecutionError::FunctionError`, matching real
/// upstream's own `min`/`max` erroring on an empty list rather than
/// returning some placeholder value.
pub fn min_binding(ftx: &FunctionContext, This(list): This<Arc<Vec<Value>>>) -> Result<Value, ExecutionError> {
    min(&list).map_err(|e| ftx.error(e))
}

pub fn max_binding(ftx: &FunctionContext, This(list): This<Arc<Vec<Value>>>) -> Result<Value, ExecutionError> {
    max(&list).map_err(|e| ftx.error(e))
}

pub fn index_of_binding(This(list): This<Arc<Vec<Value>>>, item: Value) -> i64 {
    index_of(&list, &item)
}

pub fn last_index_of_binding(This(list): This<Arc<Vec<Value>>>, item: Value) -> i64 {
    last_index_of(&list, &item)
}

/// Real upstream's own real fold (`sum(init)`'s own `Adder`-trait
/// accumulation): sums every element via `Value`'s own real `Add`
/// (`impl Add<Value> for Value { type Output = Result<Value,
/// ExecutionError>; }`, confirmed directly against the published
/// `cel` crate docs before writing this — not assumed) — a genuinely
/// unsupported pair (e.g. summing a string) surfaces `Value::add`'s own
/// real `ExecutionError`, not a silent no-op. **Named, honest
/// divergence**: real upstream picks the empty-list zero value from its
/// own compile-time overload (`int`/`uint`/`double`/`duration`, each with
/// its own real zero); this crate's own binding has no type checker to
/// know which was intended, so an empty list always sums to
/// [`Value::Int`]`(0)` — a real, incorrect answer for a
/// duration-typed empty sum specifically (`Value::Int(0) != Value::
/// Duration(0)`), not just a cosmetic difference.
pub fn sum(list: &[Value]) -> Result<Value, ExecutionError> {
    let mut acc: Option<Value> = None;
    for item in list {
        acc = Some(match acc {
            None => item.clone(),
            Some(running) => (running + item.clone())?,
        });
    }
    Ok(acc.unwrap_or(Value::Int(0)))
}

pub fn sum_binding(This(list): This<Arc<Vec<Value>>>) -> Result<Value, ExecutionError> {
    sum(&list)
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

    #[test]
    fn min_and_max_find_the_real_extremes() {
        let list = [Value::Int(3), Value::Int(1), Value::Int(2)];
        assert_eq!(min(&list).unwrap(), Value::Int(1));
        assert_eq!(max(&list).unwrap(), Value::Int(3));
    }

    #[test]
    fn min_and_max_on_a_single_element_list_return_that_element() {
        let list = [Value::Int(5)];
        assert_eq!(min(&list).unwrap(), Value::Int(5));
        assert_eq!(max(&list).unwrap(), Value::Int(5));
    }

    #[test]
    fn min_and_max_on_an_empty_list_are_real_errors() {
        assert!(min(&[]).is_err());
        assert!(max(&[]).is_err());
    }

    #[test]
    fn min_and_max_work_on_strings_too() {
        let list = [Value::String(Arc::new("b".to_string())), Value::String(Arc::new("a".to_string())), Value::String(Arc::new("c".to_string()))];
        assert_eq!(min(&list).unwrap(), Value::String(Arc::new("a".to_string())));
        assert_eq!(max(&list).unwrap(), Value::String(Arc::new("c".to_string())));
    }

    #[test]
    fn index_of_finds_the_first_real_match() {
        let list = [Value::Int(1), Value::Int(2), Value::Int(2), Value::Int(3)];
        assert_eq!(index_of(&list, &Value::Int(2)), 1);
    }

    #[test]
    fn last_index_of_finds_the_last_real_match() {
        let list = [Value::Int(1), Value::Int(2), Value::Int(2), Value::Int(3)];
        assert_eq!(last_index_of(&list, &Value::Int(2)), 2);
    }

    #[test]
    fn index_of_and_last_index_of_return_negative_one_when_not_found() {
        let list = [Value::Int(1), Value::Int(2)];
        assert_eq!(index_of(&list, &Value::Int(9)), -1);
        assert_eq!(last_index_of(&list, &Value::Int(9)), -1);
    }

    #[test]
    fn index_of_on_an_empty_list_is_negative_one() {
        assert_eq!(index_of(&[], &Value::Int(1)), -1);
    }

    #[test]
    fn sum_adds_every_real_element() {
        let list = [Value::Int(1), Value::Int(2), Value::Int(3)];
        assert_eq!(sum(&list).unwrap(), Value::Int(6));
    }

    #[test]
    fn sum_of_an_empty_list_defaults_to_int_zero() {
        assert_eq!(sum(&[]).unwrap(), Value::Int(0));
    }

    #[test]
    fn sum_works_on_floats_too() {
        let list = [Value::Float(1.5), Value::Float(2.5)];
        assert_eq!(sum(&list).unwrap(), Value::Float(4.0));
    }

    #[test]
    fn sum_of_a_genuinely_unsupported_pair_is_a_real_error_not_a_silent_no_op() {
        let list = [Value::String(Arc::new("a".to_string())), Value::Int(1)];
        assert!(sum(&list).is_err());
    }
}
