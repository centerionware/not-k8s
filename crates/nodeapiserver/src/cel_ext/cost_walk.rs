//! The real `cost()` AST-walking dispatcher (`checker/cost.go`'s own
//! `(*coster).cost`/`costCall`/`functionCost`, fetched and read
//! directly) that turns a compiled CEL expression into an actual
//! [`super::cost::CostEstimate`] — recursively summing each
//! subexpression's own cost, plus real upstream's own fixed base cost
//! per node kind, and (for a handful of real, unambiguous string
//! functions) a real per-unit cost scaled by the operand's own
//! schema-derived [`super::cost::SizeEstimate`].
//!
//! # Real, deliberate scope: which `Call`s get a real formula
//!
//! Real upstream's own `functionCost` dispatches by `overloadID` — a
//! type-specialized string cel-go's own type-checker assigns
//! (`add_string` vs. `add_int64` vs. `add_list`, distinct overloads of
//! the same `+` operator). **This crate's parser has no type checker at
//! all**, confirmed directly in the vendored source: `a + b` compiles to
//! the identical `Call{func_name: "_+_"}` node regardless of whether `a`
//! and `b` are numbers, strings, or lists — real upstream's own
//! `overloads.AddString`/`AddList` (O(n)) vs. plain numeric `+` (O(1))
//! genuinely cannot be told apart at this AST level.
//!
//! Asked explicitly which way to resolve this for every operator real
//! upstream costs differently by type (`+`, `==`/`!=`, `<`/`>`/`<=`/
//! `>=`): **treat them as the cheap O(1) case**, matching real
//! upstream's own fallback for a genuinely-unrecognized function — a
//! deliberate choice (not an oversight) to avoid over-rejecting a
//! purely-numeric rule at CRD-acceptance time, accepting that a
//! string/list-heavy use of these operators is under-counted the same
//! way any other not-yet-specially-costed function already is.
//!
//! What *is* real and type-unambiguous regardless of the missing type
//! checker: a **named method call** (`str.matches(...)`,
//! `str.contains(...)`, `str.startsWith(...)`, `str.endsWith(...)`) —
//! these function *names* only ever apply to a string target in real
//! CEL's own standard library, so there's no ambiguity to resolve.
//! Those get real upstream's own exact formula. Everything else
//! (including every ambiguous operator above, and any function this
//! slice doesn't yet name) falls to real upstream's own documented O(1)
//! default: `FixedCostEstimate(1)` plus the sum of every argument's own
//! cost.
//!
//! `Comprehension` is still not dispatched — `CostEstimate::unknown()`,
//! its own real follow-up slice (the loop-cost multiplication, needing
//! this module's own [`Coster`] to track the iteration variable's
//! bound path through [`super::path::Scope`], which now exists here for
//! exactly that reason).

use super::cost::{CostEstimate, SizeEstimate, CONST_COST, LIST_CREATE_BASE_COST, MAP_CREATE_BASE_COST, REGEX_STRING_LENGTH_COST_FACTOR, SELECT_AND_IDENT_COST, STRING_TRAVERSAL_COST_FACTOR, STRUCT_CREATE_BASE_COST};
use super::decl_type::{estimate_size, DeclType};
use super::path::{resolve_path, Scope};
use cel::common::ast::{CallExpr, EntryExpr, Expr, LiteralValue};
use cel::IdedExpr;
use std::collections::HashMap;

/// Real upstream's own default `presenceTestCost` (the `coster` struct's
/// own zero-value init, `FixedCostEstimate(1)`) — numerically identical
/// to [`SELECT_AND_IDENT_COST`] today, kept as its own named constant
/// since they're conceptually distinct in real upstream (its own
/// `PresenceTestHasCost(false)` option can zero this one out
/// independently of the other — not modeled here, named honestly: this
/// crate has no `CostOption`-equivalent configuration surface yet).
const PRESENCE_TEST_COST: u64 = 1;

/// Estimates the real cost of `expr` with no CRD schema available — the
/// convenience most callers want (`x-kubernetes-validations` rules
/// against a schema use [`Coster`] directly instead, so schema-driven
/// size lookups actually resolve). Equivalent to `Coster::new(None).
/// cost(expr)`.
pub fn cost(expr: &IdedExpr) -> CostEstimate {
    Coster::new(None).cost(expr)
}

/// Real upstream's own `coster` — the stateful walker `cost()` needs
/// once any real size lookup is involved (a `Call` to a real string
/// function, a future `Comprehension`): `root` is the CRD's own
/// compiled schema (`None` when no schema is available, e.g. a
/// non-CRD context), `scope` tracks comprehension-variable path
/// bindings (empty until `Comprehension`'s own follow-up slice pushes
/// one), and `computed_sizes` is a per-expression-id memo — real
/// upstream's own `computedSizes` map, avoiding recomputing the same
/// subexpression's size more than once.
pub struct Coster<'a> {
    root: Option<&'a DeclType>,
    scope: Scope,
    computed_sizes: HashMap<u64, SizeEstimate>,
}

impl<'a> Coster<'a> {
    pub fn new(root: Option<&'a DeclType>) -> Self {
        Self { root, scope: Scope::new(), computed_sizes: HashMap::new() }
    }

    /// Real upstream's own `(*coster).cost`.
    pub fn cost(&mut self, expr: &IdedExpr) -> CostEstimate {
        match &expr.expr {
            Expr::Literal(_) => CostEstimate::fixed(CONST_COST),
            Expr::Ident(_) => CostEstimate::fixed(SELECT_AND_IDENT_COST),
            Expr::Select(sel) => {
                let operand_cost = self.cost(&sel.operand);
                if sel.test {
                    operand_cost.add(CostEstimate::fixed(PRESENCE_TEST_COST))
                } else {
                    operand_cost.add(CostEstimate::fixed(SELECT_AND_IDENT_COST))
                }
            }
            Expr::List(list) => list.elements.iter().fold(CostEstimate::default(), |sum, e| sum.add(self.cost(e))).add(CostEstimate::fixed(LIST_CREATE_BASE_COST)),
            Expr::Map(map) => map
                .entries
                .iter()
                .fold(CostEstimate::default(), |sum, entry| match &entry.expr {
                    EntryExpr::MapEntry(e) => sum.add(self.cost(&e.key)).add(self.cost(&e.value)),
                    EntryExpr::StructField(_) => sum,
                })
                .add(CostEstimate::fixed(MAP_CREATE_BASE_COST)),
            Expr::Struct(st) => st
                .entries
                .iter()
                .fold(CostEstimate::default(), |sum, entry| match &entry.expr {
                    EntryExpr::StructField(f) => sum.add(self.cost(&f.value)),
                    EntryExpr::MapEntry(_) => sum,
                })
                .add(CostEstimate::fixed(STRUCT_CREATE_BASE_COST)),
            Expr::Call(call) => self.cost_call(call),
            // Named, honest gap -- see this module's own doc comment.
            Expr::Comprehension(_) => CostEstimate::unknown(),
            Expr::Unspecified => CostEstimate::default(),
        }
    }

    /// Real upstream's own `costCall` — target cost (for a member-style
    /// call) plus the sum of every argument's own cost, plus the call's
    /// own intrinsic cost ([`Self::function_cost`]).
    fn cost_call(&mut self, call: &CallExpr) -> CostEstimate {
        let arg_costs: Vec<CostEstimate> = call.args.iter().map(|a| self.cost(a)).collect();
        let arg_cost_sum = arg_costs.iter().fold(CostEstimate::default(), |sum, c| sum.add(*c));

        let mut sum = CostEstimate::default();
        if let Some(target) = &call.target {
            sum = sum.add(self.cost(target));
        }
        sum.add(self.function_cost(call, arg_cost_sum))
    }

    /// Real upstream's own `functionCost` — see this module's own doc
    /// comment for exactly which real per-function formulas are ported
    /// here and why the rest (every ambiguous operator, any
    /// unrecognized function) falls to the same real O(1) default real
    /// upstream itself uses for a function it has no specific estimate
    /// for.
    fn function_cost(&mut self, call: &CallExpr, arg_cost_sum: CostEstimate) -> CostEstimate {
        match call.func_name.as_str() {
            // Real upstream's own `overloads.Matches`/`MatchesString` —
            // https://swtch.com/~rsc/regexp/regexp1.html: the string's
            // own traversal cost times the regex pattern's own length
            // cost. `str.matches(pattern)` (member form) is the only
            // real shape a CRD validation rule ever actually uses;
            // real upstream's own non-member `matches(str, pattern)`
            // form is a legacy alternate spelling this crate's own
            // grammar doesn't produce a distinct AST shape for anyway.
            "matches" => {
                let (Some(target), Some(pattern)) = (&call.target, call.args.first()) else {
                    return CostEstimate::fixed(1).add(arg_cost_sum);
                };
                // Real upstream's own "+1 to prevent the product being
                // zero when the string is empty but the regex is still
                // expensive" adjustment.
                let str_cost = self.size_or_unknown(target).add(SizeEstimate::fixed(1)).multiply_by_cost_factor(STRING_TRAVERSAL_COST_FACTOR);
                let regex_cost = self.size_or_unknown(pattern).multiply_by_cost_factor(REGEX_STRING_LENGTH_COST_FACTOR);
                str_cost.multiply(regex_cost).add(arg_cost_sum)
            }
            // Real upstream's own `overloads.ContainsString`.
            "contains" => {
                let (Some(target), Some(substr)) = (&call.target, call.args.first()) else {
                    return CostEstimate::fixed(1).add(arg_cost_sum);
                };
                let str_cost = self.size_or_unknown(target).multiply_by_cost_factor(STRING_TRAVERSAL_COST_FACTOR);
                let substr_cost = self.size_or_unknown(substr).multiply_by_cost_factor(STRING_TRAVERSAL_COST_FACTOR);
                str_cost.multiply(substr_cost).add(arg_cost_sum)
            }
            // Real upstream's own `overloads.StartsWithString`/
            // `EndsWithString` — a single traversal of the (real
            // upstream's own comment: shorter, in practice) argument
            // being searched for, not the target.
            "startsWith" | "endsWith" => {
                let Some(arg) = call.args.first() else {
                    return CostEstimate::fixed(1).add(arg_cost_sum);
                };
                self.size_or_unknown(arg).multiply_by_cost_factor(STRING_TRAVERSAL_COST_FACTOR).add(arg_cost_sum)
            }
            // Real upstream's own O(1) default (`functionCost`'s own
            // trailing comment: "+/- 50% of a base cost unit") — every
            // ambiguous ("_+_", "_==_", "_!=_", "_<_", "_>_", "_<=_",
            // "_>=_") and every not-yet-named function alike.
            _ => CostEstimate::fixed(1).add(arg_cost_sum),
        }
    }

    fn size_or_unknown(&mut self, expr: &IdedExpr) -> SizeEstimate {
        self.compute_size(expr).unwrap_or_else(SizeEstimate::unknown)
    }

    /// Real upstream's own `computeSize` — an already-cached size, else
    /// a literal/list/map's own exact size, else a schema-driven lookup
    /// through [`super::path::resolve_path`]/[`estimate_size`]. `None`
    /// (not [`SizeEstimate::unknown`]) when nothing resolves it at
    /// all — callers that need a concrete value use
    /// [`Self::size_or_unknown`] instead.
    fn compute_size(&mut self, expr: &IdedExpr) -> Option<SizeEstimate> {
        if let Some(size) = self.computed_sizes.get(&expr.id) {
            return Some(*size);
        }
        if let Some(size) = compute_expr_size(&expr.expr) {
            self.computed_sizes.insert(expr.id, size);
            return Some(size);
        }
        let root = self.root?;
        let path = resolve_path(expr, &self.scope)?;
        let size = estimate_size(root, &path)?;
        self.computed_sizes.insert(expr.id, size);
        Some(size)
    }
}

/// Real upstream's own `computeExprSize` — an expression whose exact
/// size is knowable from its own literal syntax alone, no schema
/// needed: a string/bytes literal's own real length, any other scalar
/// literal (always size `1`), or an inline list/map literal's own
/// element count.
fn compute_expr_size(expr: &Expr) -> Option<SizeEstimate> {
    match expr {
        Expr::Literal(lit) => match lit {
            LiteralValue::String(s) => Some(SizeEstimate::fixed(s.inner().chars().count() as u64)),
            LiteralValue::Bytes(b) => Some(SizeEstimate::fixed(b.inner().len() as u64)),
            LiteralValue::Boolean(_) | LiteralValue::Double(_) | LiteralValue::Int(_) | LiteralValue::Null | LiteralValue::UInt(_) => Some(SizeEstimate::fixed(1)),
        },
        Expr::List(list) => Some(SizeEstimate::fixed(list.elements.len() as u64)),
        Expr::Map(map) => Some(SizeEstimate::fixed(map.entries.len() as u64)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cel::Program;
    use serde_json::json;

    fn compile(expr: &str) -> IdedExpr {
        Program::compile(expr).unwrap().expression().clone()
    }

    #[test]
    fn a_literal_costs_nothing() {
        assert_eq!(cost(&compile("42")), CostEstimate::fixed(CONST_COST));
    }

    #[test]
    fn a_bare_identifier_costs_one() {
        assert_eq!(cost(&compile("self")), CostEstimate::fixed(SELECT_AND_IDENT_COST));
    }

    #[test]
    fn a_select_chain_costs_one_per_hop_plus_the_identifier() {
        assert_eq!(cost(&compile("self.spec.replicas")), CostEstimate::fixed(3));
    }

    #[test]
    fn a_presence_test_costs_the_operand_plus_one() {
        assert_eq!(cost(&compile("has(self.spec.replicas)")), CostEstimate::fixed(3));
    }

    #[test]
    fn an_empty_list_costs_just_the_base_cost() {
        assert_eq!(cost(&compile("[]")), CostEstimate::fixed(LIST_CREATE_BASE_COST));
    }

    #[test]
    fn a_list_of_identifiers_sums_their_own_real_cost() {
        assert_eq!(cost(&compile("[self, self]")), CostEstimate::fixed(2 + LIST_CREATE_BASE_COST));
    }

    #[test]
    fn an_empty_map_costs_just_the_base_cost() {
        assert_eq!(cost(&compile("{}")), CostEstimate::fixed(MAP_CREATE_BASE_COST));
    }

    #[test]
    fn a_comprehension_is_still_unknown() {
        assert_eq!(cost(&compile("self.list.all(x, x > 0)")), CostEstimate::unknown());
    }

    #[test]
    fn an_ambiguous_operator_falls_to_the_cheap_o1_default() {
        // self (1) + self (1), default O(1) call cost (1) = 3 -- not the
        // string-shaped O(n) formula, per the user's own explicit choice.
        assert_eq!(cost(&compile("self + self")), CostEstimate::fixed(3));
    }

    #[test]
    fn equality_also_falls_to_the_cheap_o1_default() {
        assert_eq!(cost(&compile("self == self")), CostEstimate::fixed(3));
    }

    #[test]
    fn an_unrecognized_function_falls_to_the_o1_default_plus_its_args() {
        // size(self): arg cost 1 (self) + call cost 1 = 2.
        assert_eq!(cost(&compile("size(self)")), CostEstimate::fixed(2));
    }

    #[test]
    fn matches_with_a_known_schema_size_uses_the_real_regex_formula() {
        let root = crate::cel_ext::decl_type::decl_type_for(&json!({
            "type": "object",
            "properties": {"name": {"type": "string", "maxLength": 10}},
        }))
        .unwrap();
        let expr = compile("self.name.matches('a+')");
        let result = Coster::new(Some(&root)).cost(&expr);
        // Not the O(1) default (2: 1 arg + 1 call) -- a real size-scaled
        // cost, strictly greater given a real maxLength bound.
        assert!(result.max > 2, "expected a size-scaled cost, got {result:?}");
    }

    #[test]
    fn matches_with_no_schema_falls_back_to_unknown_size_but_still_a_real_formula() {
        // No schema at all -- size_or_unknown returns SizeEstimate::unknown()
        // for the target, still runs the real regex formula (not the O(1)
        // default), producing a cost far beyond any real budget
        // (RuntimeCELCostBudget is 10_000_000) even after the unknown
        // size is scaled down by STRING_TRAVERSAL_COST_FACTOR and back up
        // by the regex's own real, small size.
        let result = cost(&compile("self.matches('a+')"));
        assert!(result.max > 10_000_000, "expected an effectively unbounded cost, got {result:?}");
    }

    #[test]
    fn contains_uses_the_real_quadratic_formula_with_a_known_schema() {
        let root = crate::cel_ext::decl_type::decl_type_for(&json!({
            "type": "object",
            "properties": {"name": {"type": "string", "maxLength": 20}},
        }))
        .unwrap();
        let expr = compile("self.name.contains('x')");
        let result = Coster::new(Some(&root)).cost(&expr);
        assert!(result.max > 2, "expected a size-scaled cost, got {result:?}");
    }

    #[test]
    fn starts_with_scales_with_the_arguments_own_exact_literal_size() {
        // The literal argument's own exact size (5 chars, from
        // computeExprSize, no schema lookup needed at all) drives the
        // cost, not the schema-bounded target: self.name (2) + the
        // literal's own real traversal cost (5 chars * 0.1/char, rounded
        // up to 1) + no extra arg cost (the literal itself is free) = 3.
        let expr = compile("self.name.startsWith('hello')");
        let result = cost(&expr);
        assert_eq!(result, CostEstimate::fixed(3));
    }
}
