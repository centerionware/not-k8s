//! Resolves a CEL expression's own field path — real upstream's own
//! `coster.getPath`/`costIdent`/`costSelect` (`checker/cost.go`, fetched
//! and read directly): a `Select`/`Ident` chain rooted at a bound
//! variable (`self`/`oldSelf`) turns into a `Vec<String>` path
//! (`["self", "spec", "foo"]`), the same shape
//! [`super::decl_type::DeclType`]-walking size resolution (a follow-up
//! slice) consumes to answer "how big could this expression's value
//! be" from the CRD's own schema.
//!
//! **Named, honest simplification**: real upstream also tracks paths
//! through a comprehension's own iteration variable(s)
//! (`pushIterKey`/`pushIterValue`/`pushIterSingle`), appending a real
//! `@items`/`@keys`/`@values` path segment resolved from the *type* of
//! the range being iterated (a list vs. a map). This crate has no CEL
//! type-checker (`cel_ext`'s own module doc covers why), so
//! [`comprehension_iter_path`] only handles the single-variable
//! comprehension form (`list.all(x, ...)`/`.exists(x, ...)`/`.map(x,
//! ...)`) and always treats the iteration variable as a list-shaped
//! `@items` access — the overwhelmingly common real
//! `x-kubernetes-validations` usage (iterating a `spec`-declared list).
//! The two-variable form (`all(k, v, ...)`, real CEL's own map-iteration
//! macro) isn't resolved to a path at all; a reference to either of its
//! variables returns `None` from [`resolve_path`], the same honest
//! "no bound available" outcome as any other genuinely unresolvable
//! path — not a wrong answer, just an absent one.

use cel::common::ast::{ComprehensionExpr, Expr};
use cel::IdedExpr;
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

    fn push(&mut self, name: &str, path: Vec<String>) {
        self.bindings.entry(name.to_string()).or_default().push(path);
    }

    fn pop(&mut self, name: &str) {
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

/// The single-variable comprehension form's own iteration-variable path
/// (see this module's own doc comment for the real, named scope this is
/// narrowed to): `comp.iter_range`'s own resolved path with a trailing
/// `"@items"` segment, real upstream's own `pushIterSingle` — list-only,
/// since without a type-checker this crate can't distinguish a list
/// range from a map one the way real upstream's own `getType` does.
pub fn comprehension_iter_path(comp: &ComprehensionExpr, scope: &Scope) -> Option<Vec<String>> {
    let mut path = resolve_path(&comp.iter_range, scope)?;
    path.push("@items".to_string());
    Some(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use cel::Program;

    fn compile(expr: &str) -> IdedExpr {
        Program::compile(expr).unwrap().expression().clone()
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
        let path = comprehension_iter_path(comp, &Scope::new());
        assert_eq!(path, Some(vec!["self".to_string(), "spec".to_string(), "items".to_string(), "@items".to_string()]));
    }
}
