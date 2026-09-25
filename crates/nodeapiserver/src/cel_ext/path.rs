//! Resolves a CEL expression's own field path — real upstream's own
//! `coster.getPath`/`costIdent`/`costSelect` (`checker/cost.go`, fetched
//! and read directly): a `Select`/`Ident` chain rooted at a bound
//! variable (`self`/`oldSelf`) turns into a `Vec<String>` path
//! (`["self", "spec", "foo"]`), the same shape
//! [`super::decl_type::DeclType`]-walking size resolution (a follow-up
//! slice) consumes to answer "how big could this expression's value
//! be" from the CRD's own schema.
//!
//! **Named, honest simplification**: [`comprehension_iter_path`] handles
//! the single-variable comprehension form for list elements and map keys
//! by resolving the range against the declared structural schema. The
//! two-variable map form (`all(k, v, ...)`) is not yet resolved to paths
//! for static costs; references to either iteration variable therefore
//! have no schema-derived size estimate.

use super::decl_type::{DeclType, Shape};
use cel::IdedExpr;
use cel::common::ast::{ComprehensionExpr, Expr};
use std::collections::HashMap;

/// A stack of variable-name -> path bindings, pushed on comprehension
/// entry and popped on exit — real upstream's own `scopes`, scoped down
/// to the one binding this module tracks per comprehension (its own
/// iteration variable; the accumulator variable is deliberately never
/// given a binding at all, matching real upstream's own `newAstNode`'s
/// "omit accumulator vars from any path" rule — this module simply never
/// calls [`Scope::push`] for one).
#[derive(Debug, Default)]
pub struct Scope {
    bindings: HashMap<String, Vec<Vec<String>>>,
}

impl Scope {
    pub fn new() -> Self {
        Self::default()
    }

    /// `pub(crate)` rather than private — [`super::cost_walk::Coster`]'s
    /// own `Comprehension` dispatch needs to push/pop a binding directly
    /// (not through [`with_binding`] below, which would need to borrow
    /// `Coster`'s own `scope` field and the rest of `Coster` mutably at
    /// the same time — a real borrow-checker conflict `with_binding`'s
    /// own closure-based API can't route around here).
    pub(crate) fn push(&mut self, name: &str, path: Vec<String>) {
        self.bindings.entry(name.to_string()).or_default().push(path);
    }

    pub(crate) fn pop(&mut self, name: &str) {
        if let Some(stack) = self.bindings.get_mut(name) {
            stack.pop();
        }
    }

    fn peek(&self, name: &str) -> Option<&Vec<String>> {
        self.bindings.get(name).and_then(|s| s.last())
    }
}

/// Resolves `expr`'s own field path — `None` when it isn't a
/// `Select`/`Ident` chain rooted at a bound variable (a literal, a
/// function call's own result, an unbound comprehension variable this
/// module's own real scope narrowing doesn't cover, ...). Real
/// upstream's own `nil`/empty-path outcome, which its own
/// `sizeEstimator.EstimateSize` already treats as "no estimate
/// available" rather than a hard error — callers here should do the
/// same.
///
/// Deliberately doesn't special-case a presence-test `Select`
/// (`has(self.foo)`, real upstream's own `sel.IsTestOnly()`) — real
/// upstream's own `costSelect` skips path tracking for one, but that's a
/// decision about *whether this particular call site should bother
/// resolving a path at all* (a presence test's own cost never depends
/// on the field's size), not something `resolve_path` itself needs to
/// know; a caller checking `IsTestOnly` first and simply not calling
/// this function for one gets the identical real outcome.
pub fn resolve_path(expr: &IdedExpr, scope: &Scope) -> Option<Vec<String>> {
    match &expr.expr {
        Expr::Ident(name) => match scope.peek(name) {
            Some(path) => Some(path.clone()),
            None => Some(vec![name.clone()]),
        },
        Expr::Select(sel) => {
            let mut path = resolve_path(&sel.operand, scope)?;
            path.push(sel.field.clone());
            Some(path)
        }
        _ => None,
    }
}

/// Runs `f` with `var_name` bound to `path` in `scope` for the duration
/// of the call — real upstream's own `pushLocalVar`/`popLocalVar` pair,
/// collapsed into one real RAII-shaped helper so a caller can't forget
/// the matching pop even on an early return out of `f`.
pub fn with_binding<R>(scope: &mut Scope, var_name: &str, path: Vec<String>, f: impl FnOnce(&mut Scope) -> R) -> R {
    scope.push(var_name, path);
    let result = f(scope);
    scope.pop(var_name);
    result
}

/// The single-variable comprehension form's iteration-variable path:
/// resolve the range against its structural schema and append the matching
/// list-element or map-key segment, mirroring upstream's `pushIterSingle`.
pub fn comprehension_iter_path(
    comp: &ComprehensionExpr,
    scope: &Scope,
    root: &DeclType,
) -> Option<Vec<String>> {
    let mut path = resolve_path(&comp.iter_range, scope)?;
    let mut current = root;
    for segment in path.iter().skip(1) {
        current = match (segment.as_str(), &current.shape) {
            ("@items", Shape::List(element)) | ("@values", Shape::Map(element)) => element,
            ("@keys", Shape::Map(_)) => return None,
            (name, Shape::Object(fields)) => fields.get(name)?,
            _ => return None,
        };
    }
    path.push(
        match &current.shape {
            Shape::List(_) => "@items",
            // Kubernetes CEL map rules commonly use the single-variable macro
            // form to validate each key, including Gateway API annotations.
            Shape::Map(_) => "@keys",
            _ => return None,
        }
        .to_string(),
    );
    Some(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn compile(expr: &str) -> IdedExpr {
        super::super::compile(expr).unwrap()
    }

    #[test]
    fn a_bare_identifier_is_its_own_one_element_path() {
        let expr = compile("self");
        assert_eq!(resolve_path(&expr, &Scope::new()), Some(vec!["self".to_string()]));
    }

    #[test]
    fn a_select_chain_resolves_to_the_full_real_path() {
        let expr = compile("self.spec.replicas");
        assert_eq!(resolve_path(&expr, &Scope::new()), Some(vec!["self".to_string(), "spec".to_string(), "replicas".to_string()]));
    }

    #[test]
    fn a_presence_test_still_resolves_the_underlying_path() {
        // Whether the caller *uses* this for a presence test is its own
        // decision (see this module's own doc comment) -- the function
        // itself doesn't refuse just because `has()` was used.
        let expr = compile("has(self.spec.replicas)");
        assert_eq!(resolve_path(&expr, &Scope::new()), Some(vec!["self".to_string(), "spec".to_string(), "replicas".to_string()]));
    }

    #[test]
    fn a_literal_has_no_path() {
        let expr = compile("42");
        assert_eq!(resolve_path(&expr, &Scope::new()), None);
    }

    #[test]
    fn a_function_calls_own_result_has_no_path() {
        let expr = compile("size(self.spec.items)");
        assert_eq!(resolve_path(&expr, &Scope::new()), None);
    }

    #[test]
    fn a_bound_variable_resolves_through_its_own_scope_binding() {
        let mut scope = Scope::new();
        let ident = compile("x");
        with_binding(&mut scope, "x", vec!["self".to_string(), "spec".to_string(), "@items".to_string()], |scope| {
            assert_eq!(resolve_path(&ident, scope), Some(vec!["self".to_string(), "spec".to_string(), "@items".to_string()]));
        });
        // Popped -- outside the binding's own scope, "x" is just itself again.
        assert_eq!(resolve_path(&ident, &scope), Some(vec!["x".to_string()]));
    }

    #[test]
    fn a_field_selected_off_a_bound_comprehension_variable_extends_its_path() {
        let mut scope = Scope::new();
        let selected = compile("x.name");
        with_binding(&mut scope, "x", vec!["self".to_string(), "spec".to_string(), "items".to_string(), "@items".to_string()], |scope| {
            assert_eq!(
                resolve_path(&selected, scope),
                Some(vec!["self".to_string(), "spec".to_string(), "items".to_string(), "@items".to_string(), "name".to_string()])
            );
        });
    }

    #[test]
    fn comprehension_iter_path_appends_the_real_items_segment() {
        let expr = compile("self.spec.items.all(x, x.enabled)");
        // Real upstream's own `.all()` macro (`parser/macros.rs`'s
        // `all_macro_expander`, confirmed directly) expands *in place*
        // at parse time into a bare `Expr::Comprehension` -- it is never
        // left wrapped in a `Call` the way a real function invocation
        // would be.
        let Expr::Comprehension(comp) = &expr.expr else { panic!("expected the .all() macro to desugar directly into a Comprehension, got {:?}", expr.expr) };
        let root = super::decl_type::decl_type_for(&serde_json::json!({
            "type": "object",
            "properties": {
                "spec": {
                    "type": "object",
                    "properties": {
                        "items": {"type": "array", "items": {"type": "string"}}
                    }
                }
            }
        }))
        .unwrap();
        let path = comprehension_iter_path(comp, &Scope::new(), &root);
        assert_eq!(path, Some(vec!["self".to_string(), "spec".to_string(), "items".to_string(), "@items".to_string()]));
    }

    #[test]
    fn single_variable_map_comprehension_tracks_keys() {
        let expr = compile("self.spec.annotations.all(key, key.matches('a+'))");
        let Expr::Comprehension(comp) = &expr.expr else { panic!("expected a CEL comprehension") };
        let root = super::decl_type::decl_type_for(&serde_json::json!({
            "type": "object",
            "properties": {
                "spec": {
                    "type": "object",
                    "properties": {
                        "annotations": {
                            "type": "object",
                            "maxProperties": 16,
                            "additionalProperties": {"type": "string", "maxLength": 4096}
                        }
                    }
                }
            }
        }))
        .unwrap();
        assert_eq!(
            comprehension_iter_path(comp, &Scope::new(), &root),
            Some(vec![
                "self".to_string(),
                "spec".to_string(),
                "annotations".to_string(),
                "@keys".to_string()
            ])
        );
    }
}
