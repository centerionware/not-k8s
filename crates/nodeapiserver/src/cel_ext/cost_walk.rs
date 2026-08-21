//! The real `cost()` AST-walking dispatcher (`checker/cost.go`'s own
//! `(*coster).cost`, fetched and read directly) that turns a compiled
//! CEL expression into an actual [`super::cost::CostEstimate`] —
//! recursively summing each subexpression's own cost, plus real
//! upstream's own fixed base cost per node kind
//! ([`super::cost::CONST_COST`]/[`super::cost::SELECT_AND_IDENT_COST`]/
//! [`super::cost::LIST_CREATE_BASE_COST`]/
//! [`super::cost::MAP_CREATE_BASE_COST`]/
//! [`super::cost::STRUCT_CREATE_BASE_COST`]).
//!
//! **Named, honest scope for this first slice**: only the "structural"
//! node kinds are dispatched here — `Literal`/`Ident`/`Select`/`List`/
//! `Map`/`Struct`. `Call` (real upstream's own `costCall`/
//! `functionCost`, a large per-builtin-function cost table) and
//! `Comprehension` (`costComprehension`, needing loop-cost
//! multiplication these six kinds don't) are each their own follow-up
//! slice — both fall back to [`super::cost::CostEstimate::unknown`]
//! here rather than silently under-costing at `0`, so a rule using
//! either is conservatively treated as unbounded rather than wrongly
//! cheap until its own real dispatch lands. This also means [`cost`]
//! doesn't yet need [`super::decl_type`]/[`super::path`]'s own
//! schema-driven size lookup at all — nothing dispatched here ever
//! consults a subexpression's *size*, only its own cost and its
//! children's — that wiring belongs to `Call`'s own follow-up slice,
//! the first node kind that actually needs a `SizeEstimate` to compute
//! its own cost (a string traversal, a comparison, ...).

use super::cost::{CostEstimate, CONST_COST, LIST_CREATE_BASE_COST, MAP_CREATE_BASE_COST, SELECT_AND_IDENT_COST, STRUCT_CREATE_BASE_COST};
use cel::common::ast::{EntryExpr, Expr};
use cel::IdedExpr;

/// Real upstream's own default `presenceTestCost` (the `coster` struct's
/// own zero-value init, `FixedCostEstimate(1)`) — numerically identical
/// to [`SELECT_AND_IDENT_COST`] today, kept as its own named constant
/// since they're conceptually distinct in real upstream (its own
/// `PresenceTestHasCost(false)` option can zero this one out
/// independently of the other — not modeled here, named honestly: this
/// crate has no `CostOption`-equivalent configuration surface yet).
const PRESENCE_TEST_COST: u64 = 1;

/// Real upstream's own `(*coster).cost` — see this module's own doc
/// comment for the real, named scope of this first slice.
pub fn cost(expr: &IdedExpr) -> CostEstimate {
    match &expr.expr {
        Expr::Literal(_) => CostEstimate::fixed(CONST_COST),
        Expr::Ident(_) => CostEstimate::fixed(SELECT_AND_IDENT_COST),
        Expr::Select(sel) => {
            let operand_cost = cost(&sel.operand);
            if sel.test {
                // Real upstream's own `IsTestOnly` branch (`has(...)`):
                // the presence check itself costs `presenceTestCost`,
                // not `selectAndIdentCost` again.
                operand_cost.add(CostEstimate::fixed(PRESENCE_TEST_COST))
            } else {
                // Real upstream only adds `selectAndIdentCost` when the
                // operand's own resolved *type* is a map/struct/
                // type-param — this crate has no type info to check
                // that against, so it applies unconditionally instead
                // (a real, named widening: every genuine field-select
                // in a real k8s CEL rule targets a map/struct anyway —
                // selecting a field off a genuine scalar is a compile
                // error in real CEL, not a case this ever needs to
                // under-count for).
                operand_cost.add(CostEstimate::fixed(SELECT_AND_IDENT_COST))
            }
        }
        Expr::List(list) => list.elements.iter().fold(CostEstimate::default(), |sum, e| sum.add(cost(e))).add(CostEstimate::fixed(LIST_CREATE_BASE_COST)),
        Expr::Map(map) => map
            .entries
            .iter()
            .fold(CostEstimate::default(), |sum, entry| match &entry.expr {
                EntryExpr::MapEntry(e) => sum.add(cost(&e.key)).add(cost(&e.value)),
                EntryExpr::StructField(_) => sum,
            })
            .add(CostEstimate::fixed(MAP_CREATE_BASE_COST)),
        Expr::Struct(st) => st
            .entries
            .iter()
            .fold(CostEstimate::default(), |sum, entry| match &entry.expr {
                EntryExpr::StructField(f) => sum.add(cost(&f.value)),
                EntryExpr::MapEntry(_) => sum,
            })
            .add(CostEstimate::fixed(STRUCT_CREATE_BASE_COST)),
        // Named, honest gap -- see this module's own doc comment.
        Expr::Call(_) | Expr::Comprehension(_) => CostEstimate::unknown(),
        Expr::Unspecified => CostEstimate::default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cel::Program;

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
        // self (1) . spec (1) . replicas (1) = 3
        assert_eq!(cost(&compile("self.spec.replicas")), CostEstimate::fixed(3));
    }

    #[test]
    fn a_presence_test_costs_the_operand_plus_one() {
        // self (1) . spec (1) = 2, plus the presence test itself (1) = 3
        assert_eq!(cost(&compile("has(self.spec.replicas)")), CostEstimate::fixed(3));
    }

    #[test]
    fn an_empty_list_costs_just_the_base_cost() {
        assert_eq!(cost(&compile("[]")), CostEstimate::fixed(LIST_CREATE_BASE_COST));
    }

    #[test]
    fn a_list_literal_sums_element_costs_plus_the_base_cost() {
        // three literals (0 each) + base cost
        assert_eq!(cost(&compile("[1, 2, 3]")), CostEstimate::fixed(LIST_CREATE_BASE_COST));
    }

    #[test]
    fn a_list_of_identifiers_sums_their_own_real_cost() {
        // self (1) + self (1) + base cost
        assert_eq!(cost(&compile("[self, self]")), CostEstimate::fixed(2 + LIST_CREATE_BASE_COST));
    }

    #[test]
    fn an_empty_map_costs_just_the_base_cost() {
        assert_eq!(cost(&compile("{}")), CostEstimate::fixed(MAP_CREATE_BASE_COST));
    }

    #[test]
    fn a_map_literal_sums_key_and_value_costs_plus_the_base_cost() {
        // key "a" (literal, 0) + value self (1), base cost
        assert_eq!(cost(&compile("{\"a\": self}")), CostEstimate::fixed(1 + MAP_CREATE_BASE_COST));
    }

    #[test]
    fn a_call_is_unknown_in_this_slice() {
        assert_eq!(cost(&compile("size(self)")), CostEstimate::unknown());
    }

    #[test]
    fn a_comprehension_is_unknown_in_this_slice() {
        assert_eq!(cost(&compile("self.list.all(x, x > 0)")), CostEstimate::unknown());
    }
}
