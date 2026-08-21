//! CEL cost estimation/budget, Kubernetes' extension libraries, and
//! type-checking against a structural schema.
//!
//! Status: **Phase 1 landed** (compile + evaluate one real CEL
//! expression, no cost accounting yet) — see `docs/APISERVER.md`'s own
//! `cel_ext` section (right after Group K) for the real, verified full
//! plan: real upstream's own budget numbers (`RuntimeCELCostBudget`/
//! `PerCallLimit`/`CheckFrequency`, ..., fetched directly from
//! `k8s.io/apiserver/pkg/apis/cel/config.go` + `pkg/cel/limits.go` —
//! genuinely new territory for this crate's own vendoring flow, which
//! otherwise only pulls protos/OpenAPI specs, not hand-written Go
//! logic), the real two-layer mechanism (static "checked cost"
//! estimation at CRD-acceptance time, separate from runtime cost
//! accounting during real evaluation), and the remaining five-phase
//! build-out. **Not safe to wire this module into any real request path
//! (Group J admission or Group K's `x-kubernetes-validations`) until
//! Phase 2 (runtime cost accounting) lands** — an unbudgeted CEL
//! evaluator reachable from a real request is a real DoS surface, not
//! hardening to add later.
//!
//! Named `cel_ext`, not `cel` — see the module-map note in `lib.rs` for why
//! (this crate also depends on the external `cel` crate).
//!
//! # The `cel` crate, and getting its API right
//!
//! `crates/nodescheduler` already depends on `cel` for a real, live,
//! already-merged use (`framework::plugins::dynamic_resources`'s own
//! `DeviceSelector.cel.expression` evaluation, DRA device matching) —
//! that code, not any external documentation, is this module's own
//! primary source of truth for the crate's real API shape
//! (`cel::Program::compile`/`cel::Context::default`/
//! `Context::add_variable`/`Program::execute`/`cel::Value::Bool`, all
//! confirmed directly against `dynamic_resources.rs`'s own working
//! code rather than assumed from docs.rs, whose auto-generated summaries
//! disagreed with each other on `Context`'s own basic shape when this
//! module was first written). `crates/nodescheduler/Cargo.toml`'s own
//! comment on its `cel` dependency, and `docs/APISERVER.md`'s Phase 0
//! entry recording that crate's `cel-interpreter` -> `cel` migration,
//! are what caught that this crate's own design pass (written before
//! checking either) had cited the wrong, now-inactive crates.io name.

use cel::{Context, Program, Value as CelValue};
use serde_json::Value;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("compiling the CEL expression failed: {0}")]
    Compile(#[from] cel::ParseErrors),
    /// `cel::Context::add_variable`'s own error type carries no useful
    /// detail beyond "this value doesn't convert" — `nodescheduler`'s
    /// own `dynamic_resources.rs` discards it the same way (`.is_err()`),
    /// not a shortcut unique to this module.
    #[error("binding {name} into the CEL evaluation context failed")]
    Bind { name: &'static str },
    #[error("evaluating the CEL expression failed: {0}")]
    Execute(#[from] cel::ExecutionError),
    /// Real upstream's own `x-kubernetes-validations` requirement: every
    /// rule must evaluate to a CEL `bool` — anything else (including a
    /// CEL runtime error that isn't a compile/execute failure, which
    /// this crate's own `Value` enum can't represent since `execute`
    /// already separates errors from values) is a real, reportable
    /// authoring mistake, not silently coerced.
    #[error("the CEL expression evaluated to {0:?}, not a bool -- x-kubernetes-validations rules must be boolean")]
    NotBool(CelValue),
}

/// Compiles `expr` and evaluates it once against `self` (the value
/// under validation) and, when given, `oldSelf` (the previous value, on
/// `UPDATE`) — real upstream's own two well-known `x-kubernetes-
/// validations` variable names (`k8s.io/apiserver/pkg/cel`'s
/// `ScopedVarName`/`OldScopedVarName`, matching what every real CRD
/// validation rule in the wild already assumes). Returns the real
/// boolean result: `true` means the rule passed, matching real
/// upstream's own `Rule.Message` semantics (a rule that evaluates
/// `false` is what triggers the violation).
///
/// **Phase 1 only — no cost accounting**: `docs/APISERVER.md`'s own
/// `cel_ext` section names this module's own doc comment's warning
/// again — this function must not be reachable from any real request
/// path until Phase 2 lands a budget in front of it.
pub fn eval_bool(expr: &str, self_value: &Value, old_self_value: Option<&Value>) -> Result<bool, Error> {
    let program = Program::compile(expr)?;
    let mut ctx = Context::default();
    ctx.add_variable("self", self_value.clone()).map_err(|_| Error::Bind { name: "self" })?;
    if let Some(old) = old_self_value {
        ctx.add_variable("oldSelf", old.clone()).map_err(|_| Error::Bind { name: "oldSelf" })?;
    }
    match program.execute(&ctx)? {
        CelValue::Bool(b) => Ok(b),
        other => Err(Error::NotBool(other)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_true_expression_passes() {
        assert_eq!(eval_bool("1 + 1 == 2", &json!({}), None).unwrap(), true);
    }

    #[test]
    fn a_false_expression_fails() {
        assert_eq!(eval_bool("1 + 1 == 3", &json!({}), None).unwrap(), false);
    }

    #[test]
    fn self_is_bound_to_the_real_value_under_validation() {
        let value = json!({"spec": {"replicas": 3}});
        assert_eq!(eval_bool("self.spec.replicas > 0", &value, None).unwrap(), true);
        assert_eq!(eval_bool("self.spec.replicas > 10", &value, None).unwrap(), false);
    }

    #[test]
    fn old_self_is_bound_only_when_given() {
        let value = json!({"spec": {"replicas": 3}});
        let old = json!({"spec": {"replicas": 1}});
        // A real update-immutability-style rule: replicas may only grow.
        assert_eq!(eval_bool("self.spec.replicas >= oldSelf.spec.replicas", &value, Some(&old)).unwrap(), true);
        assert_eq!(eval_bool("oldSelf.spec.replicas < self.spec.replicas", &value, Some(&old)).unwrap(), true);
    }

    #[test]
    fn referencing_old_self_without_supplying_it_is_a_real_compile_or_execute_error_not_a_panic() {
        let value = json!({"spec": {}});
        assert!(eval_bool("oldSelf.spec.replicas > 0", &value, None).is_err());
    }

    #[test]
    fn a_malformed_expression_is_a_real_compile_error() {
        let err = eval_bool("this is not valid cel (((", &json!({}), None).unwrap_err();
        assert!(matches!(err, Error::Compile(_)), "expected Error::Compile, got {err:?}");
    }

    #[test]
    fn a_non_boolean_result_is_a_named_error_not_a_silent_truthy_coercion() {
        let err = eval_bool("self.spec.replicas", &json!({"spec": {"replicas": 3}}), None).unwrap_err();
        assert!(matches!(err, Error::NotBool(_)), "expected Error::NotBool, got {err:?}");
    }

    #[test]
    fn a_string_field_comparison_works_end_to_end() {
        let value = json!({"metadata": {"name": "widget-1"}});
        assert_eq!(eval_bool(r#"self.metadata.name.startsWith("widget-")"#, &value, None).unwrap(), true);
    }
}
