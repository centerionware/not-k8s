//! Wires [`super::cost_walk`]'s own `cost()` into a real accept/reject
//! decision for one `x-kubernetes-validations` rule — a faithful, but
//! deliberately scoped, port of real upstream's own CRD-acceptance-time
//! static cost check (`k8s.io/apiextensions-apiserver/pkg/apis/
//! apiextensions/validation/validation.go`'s own `ValidateCustomResourceDefinitionOpenAPISchema`,
//! fetched and read directly): a rule whose own worst-case cost exceeds
//! `StaticEstimatedCostLimit` is rejected before the CRD is ever
//! accepted, real upstream's own `422`-shaped `Forbidden` field error
//! (`"estimated rule cost ... exceeds budget by a factor of ..."`).
//!
//! # Named, honest scope — what real upstream additionally does that
//! this doesn't yet
//!
//! Real upstream's own comparison isn't against the rule's raw
//! [`super::cost::CostEstimate`] alone — it's `cr.MaxCost *
//! cardinalityCost.MaxCardinality`, real upstream's own accounting for
//! a rule *nested* under an array/map schema potentially running once
//! per element/entry, not just once (`getExpressionCost`, confirmed
//! directly). This crate has no `CELSchemaContext`/`MaxCardinality`
//! tracking yet — that's real, separate, not-yet-started work (it needs
//! propagating a real "how many times could the enclosing structure
//! repeat this node" number down the whole schema tree, distinct from
//! [`super::decl_type::DeclType::max_elements`], which only bounds *one*
//! node's own element count, not the product of every ancestor's own
//! bound). This module's own [`check_rule_cost`] compares a rule's raw,
//! single-evaluation cost only — a real, useful check on its own (it
//! still catches a rule that's simply too expensive to run even once),
//! just not yet the full real picture for a rule nested deep inside a
//! large repeating structure.
//!
//! Also not ported: real upstream's own static `ast.OutputType() !=
//! cel.BoolType` rejection — this crate's parser has no type checker,
//! so "does this rule actually evaluate to a bool" can only be
//! discovered at runtime ([`super::eval_bool`]'s own `Error::NotBool`),
//! not rejected at CRD-acceptance time the way real upstream can.
//!
//! **Not yet wired into any real CRD-acceptance request path** — this
//! module is the pure decision primitive itself; finding where in
//! `apiextensions`'s own CRD-establishing flow to call it from is
//! separate, not-yet-started work.

use super::cost::CostEstimate;
use super::cost_walk::Coster;
use super::decl_type::DeclType;

/// Real upstream's own `StaticEstimatedCostLimit`
/// (`pkg/apis/apiextensions/validation/validation.go`, confirmed
/// directly) — numerically identical to `RuntimeCELCostBudget`
/// (already documented in `docs/APISERVER.md`'s own `cel_ext` section),
/// but a conceptually distinct constant in real upstream (one bounds a
/// single rule's own *estimated worst case* at CRD-acceptance time, the
/// other bounds the *actual accumulated* runtime cost of evaluating
/// every rule against one real object).
pub const STATIC_ESTIMATED_COST_LIMIT: u64 = 10_000_000;

#[derive(Debug, Clone, PartialEq)]
pub enum RuleCostError {
    /// The rule itself doesn't even parse — real upstream's own
    /// `"compilation failed: ..."` `Invalid` field error.
    Compile(String),
    /// The rule parses, but its own real worst-case cost exceeds
    /// [`STATIC_ESTIMATED_COST_LIMIT`] — real upstream's own
    /// `"estimated rule cost ... exceeds budget by a factor of ..."`
    /// `Forbidden` field error, `estimated_cost`/`limit` here standing
    /// in for that formatted message (`server::listener`'s own
    /// `Status` builders already have the real "format a violation
    /// message" convention this maps onto, not duplicated here).
    TooExpensive { estimated_cost: u64, limit: u64 },
}

/// Real upstream's own single-rule static cost check, scoped as this
/// module's own doc comment describes. `root` is the CRD version's own
/// already-converted schema (`decl_type::decl_type_for`, computed once
/// and reused across every rule in the same schema — real upstream's
/// own `CELSchemaContext.TypeInfo()` caching, mirrored here by simply
/// taking it as a parameter instead of recomputing it per call).
pub fn check_rule_cost(root: &DeclType, rule: &str) -> Result<CostEstimate, RuleCostError> {
    let program = cel::Program::compile(rule).map_err(|e| RuleCostError::Compile(e.to_string()))?;
    let cost = Coster::new(Some(root)).cost(program.expression());
    if cost.max > STATIC_ESTIMATED_COST_LIMIT {
        return Err(RuleCostError::TooExpensive { estimated_cost: cost.max, limit: STATIC_ESTIMATED_COST_LIMIT });
    }
    Ok(cost)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn root() -> DeclType {
        super::super::decl_type::decl_type_for(&json!({
            "type": "object",
            "properties": {
                "name": {"type": "string", "maxLength": 20},
                "tags": {"type": "array", "items": {"type": "string", "maxLength": 10}, "maxItems": 100},
            },
        }))
        .unwrap()
    }

    #[test]
    fn a_cheap_rule_is_accepted() {
        let result = check_rule_cost(&root(), "self.name == 'x'");
        assert!(result.is_ok(), "expected an acceptable cost, got {result:?}");
    }

    #[test]
    fn an_unparseable_rule_is_a_real_compile_error() {
        let result = check_rule_cost(&root(), "this is not valid cel (((");
        assert!(matches!(result, Err(RuleCostError::Compile(_))), "expected Compile, got {result:?}");
    }

    #[test]
    fn a_rule_matching_a_field_the_schema_does_not_declare_is_rejected() {
        // "extra" isn't a declared property -- the path resolves
        // structurally, but estimate_size can't walk it against the
        // schema, so size_or_unknown falls all the way back to
        // SizeEstimate::unknown(), comfortably exceeding the real limit
        // once run through the real matches() cost formula.
        let result = check_rule_cost(&root(), "self.extra.matches('a+')");
        assert!(matches!(result, Err(RuleCostError::TooExpensive { .. })), "expected TooExpensive, got {result:?}");
    }

    #[test]
    fn a_rule_matching_a_schema_bounded_short_string_is_accepted() {
        let result = check_rule_cost(&root(), "self.name.matches('a+')");
        assert!(result.is_ok(), "a short, schema-bounded field's own matches() should stay well under budget, got {result:?}");
    }

    #[test]
    fn the_too_expensive_error_carries_the_real_limit_and_a_cost_that_exceeds_it() {
        let err = check_rule_cost(&root(), "self.extra.matches('a+')").unwrap_err();
        let RuleCostError::TooExpensive { estimated_cost, limit } = err else {
            panic!("expected TooExpensive, got {err:?}");
        };
        assert_eq!(limit, STATIC_ESTIMATED_COST_LIMIT);
        assert!(estimated_cost > limit);
    }
}
