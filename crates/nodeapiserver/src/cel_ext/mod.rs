//! CEL cost estimation/budget, Kubernetes' extension libraries, and
//! type-checking against a structural schema.
//!
//! Status: **Phase 1 done** (compile + evaluate one real CEL
//! expression); **Phase 2 partially done** (`eval_bool_with_deadline` —
//! a real wall-clock deadline, this build's own stand-in for real
//! upstream's per-operation cost accounting, which needs interpreter
//! hooks the `cel` crate doesn't expose at all); **Phase 3's own
//! `cost()` dispatcher is now fully landed** (`cost` — the
//! `SizeEstimate`/`CostEstimate` arithmetic primitives; `decl_type` — a
//! CRD's own runtime schema converted into the real `DeclType` tree,
//! plus `decl_type::estimate_size`, the path-walk lookup; `path` —
//! resolves a CEL expression's own `Select`/`Ident` chain (and a
//! single-variable comprehension's own iteration path) into the field
//! path `estimate_size` consumes; `cost_walk` — the actual `cost()`
//! AST-walking dispatcher itself, real upstream's own `(*coster).cost`/
//! `costCall`/`functionCost`/`costComprehension`, every node kind now
//! dispatched: structural nodes, `Call` for the real unambiguous string
//! functions (`matches`/`contains`/`startsWith`/`endsWith`) plus the
//! real O(1) default for everything else (including every operator this
//! crate's type-checker-free AST can't type-specialize — `+`/`==`/`<`/
//! etc., a deliberate choice, not an oversight), and `Comprehension`'s
//! own real loop-cost multiplication — see that module's own doc
//! comment for the exact real scope and the one named gap
//! (`cel.bind()`'s own distinct cost shape isn't detected)); `budget` —
//! `check_rule_cost`, the real accept/reject decision for one rule
//! against [`budget::STATIC_ESTIMATED_COST_LIMIT`] (real upstream's own
//! `StaticEstimatedCostLimit`), scoped to a single rule's own raw cost —
//! **not yet accounting for real upstream's own `MaxCardinality`
//! multiplication** (a rule nested under a repeating array/map could
//! run once per element; this crate has no cardinality-propagation
//! concept yet, see that module's own doc comment). **Now wired into a
//! real CRD-acceptance request path**: `apiextensions::cel_validations`
//! recursively walks a `CustomResourceDefinition`'s own declared schema
//! (any nesting level, not just the root) and `server::rest::create`/
//! `update` both reject a `CustomResourceDefinition` write with a real
//! `422` when any declared rule's own static cost exceeds budget —
//! CEL Phase 3's own real closing milestone: a client authoring a
//! runaway `x-kubernetes-validations` rule finds out at CRD-acceptance
//! time now, not the first time some real custom resource instance
//! trips it.
//!
//! **Phase 4 started**: `apiextensions::cel_evaluate::validate_object`
//! is the real *runtime* half — actually running a schema's own
//! `x-kubernetes-validations` rules against a real custom resource
//! instance's own value (not just checking they're affordable), `self`/
//! `oldSelf` bound per schema level, each rule capped by
//! [`eval_bool_with_deadline`]'s own wall-clock stand-in
//! (`PerCallLimit`'s real ~0.1s). Wired into `server::rest::create`/
//! `update`/`patch_persist`'s existing CRD branches — a real custom
//! resource that fails its own declared rule now gets a real `422` with
//! the rule's own declared `message`. **Named, honest gap**: no
//! aggregate `RuntimeCELCostBudget` wall-clock/cost ceiling across every
//! rule evaluated for one object — each rule is capped individually,
//! not the sum, since this crate has no real runtime cost-accounting
//! mechanism (Phase 2's own still-open gap) to enforce a shared budget
//! against. See `docs/APISERVER.md`'s own
//! `cel_ext` section (right after Group K) for the real, verified full
//! plan: real upstream's own budget numbers
//! (`RuntimeCELCostBudget`/`PerCallLimit`/`CheckFrequency`, ..., fetched
//! directly from `k8s.io/apiserver/pkg/apis/cel/config.go` +
//! `pkg/cel/limits.go` — genuinely new territory for this crate's own
//! vendoring flow, which otherwise only pulls protos/OpenAPI specs, not
//! hand-written Go logic), the real two-layer mechanism (static "checked
//! cost" estimation at CRD-acceptance time, separate from runtime cost
//! accounting during real evaluation, which a wall-clock deadline alone
//! only partially covers — see [`eval_bool_with_deadline`]'s own doc
//! comment for the real, named gap), and the remaining phases.
//! **Still not safe to wire this module into any real request path**
//! (Group J admission or Group K's `x-kubernetes-validations`) — a
//! deadline alone isn't real upstream's own guarantee, and static
//! checked-cost estimation (Phase 3) isn't wired to an actual CEL
//! expression's AST yet, only its own arithmetic primitives are landed
//! so far.
//!
//! Named `cel_ext`, not `cel` — see the module-map note in `lib.rs` for why
//! (this crate also depends on the external `cel` crate).
//!
//! **Group K point 6 started**: `kubernetes_lists::is_sorted`/
//! `is_sorted_binding` is this crate's first real Kubernetes CEL
//! extension function (`list.isSorted()`, `k8s.io/apiserver/pkg/cel/
//! library/lists.go`'s own `kubernetes.lists` library, fetched and read
//! directly) — `register_kubernetes_extensions` wires it onto every
//! `Context` this module builds via `cel::Context::add_function`
//! (`cel-rust`'s own real custom-function registration API, confirmed
//! against that crate's own `example/src/functions.rs`, which the
//! published docs don't render). Real upstream's own library also has
//! `sum`/`min`/`max`/`indexOf`/`lastIndexOf`/`includes` — separate,
//! not-yet-started work, named honestly rather than silently folded in.
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

pub mod budget;
pub mod cost;
pub mod cost_walk;
pub mod decl_type;
pub mod kubernetes_lists;
pub mod path;

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
    /// [`eval_string_with_vars`]'s own real requirement — real upstream's
    /// own `messageExpression` (and `auditAnnotations[].valueExpression`)
    /// must evaluate to a CEL `string`; anything else is a real,
    /// reportable authoring mistake, not silently stringified.
    #[error("the CEL expression evaluated to {0:?}, not a string -- messageExpression must be a string")]
    NotString(CelValue),
    /// [`eval_bool_with_deadline`]'s own real cost-budget enforcement —
    /// see that function's own doc comment for exactly what this does
    /// and doesn't guarantee.
    #[error("CEL evaluation did not complete within its deadline")]
    DeadlineExceeded,
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
/// **No cost budget of its own — see [`eval_bool_with_deadline`]**: this
/// function alone must never be reachable from a real request path
/// (`docs/APISERVER.md`'s own `cel_ext` section states this repeatedly);
/// it exists as the pure, directly-testable core the budgeted wrapper
/// below calls.
pub fn eval_bool(expr: &str, self_value: &Value, old_self_value: Option<&Value>) -> Result<bool, Error> {
    let mut vars = vec![("self", self_value)];
    if let Some(old) = old_self_value {
        vars.push(("oldSelf", old));
    }
    eval_bool_with_vars(expr, &vars)
}

/// The general form [`eval_bool`] is a convenience wrapper around:
/// compiles `expr`, binds every `(name, value)` pair in `vars`,
/// evaluates once. Real upstream's own `matchConditions`
/// (`k8s.io/apiserver/pkg/admission/plugin/webhook/matchconditions`,
/// `ValidatingAdmissionPolicy`'s own `spec.validations`, ...) binds a
/// wholly different real variable set (`object`/`oldObject`/`request`/
/// `params`, not `self`/`oldSelf`) — this is the shared primitive both
/// real variable-naming conventions this crate supports are built from,
/// rather than a second copy of the same compile-bind-execute sequence.
/// Registers this crate's own real Kubernetes CEL extension functions
/// (currently just [`kubernetes_lists::is_sorted_binding`] — see that
/// module's own doc comment for the named, honest scope) onto a fresh
/// [`Context`] — called by every real entry point below so a rule can
/// use them regardless of which variable-naming convention it's
/// evaluated through.
fn register_kubernetes_extensions(ctx: &mut Context) {
    ctx.add_function("isSorted", kubernetes_lists::is_sorted_binding);
}

pub fn eval_bool_with_vars(expr: &str, vars: &[(&'static str, &Value)]) -> Result<bool, Error> {
    let program = Program::compile(expr)?;
    let mut ctx = Context::default();
    register_kubernetes_extensions(&mut ctx);
    for (name, value) in vars.iter().copied() {
        ctx.add_variable(name, value.clone()).map_err(|_| Error::Bind { name })?;
    }
    match program.execute(&ctx)? {
        CelValue::Bool(b) => Ok(b),
        other => Err(Error::NotBool(other)),
    }
}

/// Phase 2: a real wall-clock deadline around [`eval_bool`] — this
/// crate's own stand-in for real upstream's per-operation cost
/// accounting (`PerCallLimit`/`RuntimeCELCostBudget`, checked every
/// `CheckFrequency` iterations inside a comprehension), which needs
/// hooks into the CEL interpreter's own evaluation loop that the `cel`
/// crate doesn't expose — confirmed by reading `env.rs`/`context.rs`
/// directly: no cost/step/fuel/interrupt concept exists anywhere in the
/// crate, not assumed absent from an incomplete search. Real upstream's
/// own comments on those same constants describe them in wall-clock
/// terms too ("~0.1 seconds", "~1 second" — `docs/APISERVER.md`'s
/// `cel_ext` section, fetched directly from `k8s.io/apiserver/pkg/apis/
/// cel/config.go`), so a deadline bounds the same real property real
/// upstream's own cost units are calibrated against, not an unrelated
/// approximation standing in for it.
///
/// **Named, honest limitation, load-bearing enough to repeat here, not
/// just in this module's own top-level doc comment**: unlike real
/// upstream's own interruption (which reclaims the CPU mid-evaluation
/// the instant the budget is exceeded), this can only bound how long the
/// *caller* waits — Rust has no safe mechanism to forcibly terminate an
/// arbitrary running thread, so the spawned evaluation thread keeps
/// running to completion (or a crash) in the background rather than
/// being killed. One pathological expression costs one leaked thread,
/// not unbounded ones — but this function does not itself limit how
/// many concurrent evaluations are in flight; a caller relying on this
/// as its *only* defense against a flood of pathological requests is
/// trusting a guarantee this function doesn't provide. Real, separate
/// rate-limiting (Group M's own APF work) is what would have to close
/// that gap, not this module.
pub fn eval_bool_with_deadline(expr: &str, self_value: &Value, old_self_value: Option<&Value>, deadline: std::time::Duration) -> Result<bool, Error> {
    let mut vars = vec![("self", self_value)];
    if let Some(old) = old_self_value {
        vars.push(("oldSelf", old));
    }
    eval_bool_with_vars_and_deadline(expr, &vars, deadline)
}

/// The general form [`eval_bool_with_deadline`] is a convenience wrapper
/// around — see [`eval_bool_with_vars`]'s own doc comment for why a
/// second real variable-naming convention needs this.
pub fn eval_bool_with_vars_and_deadline(expr: &str, vars: &[(&'static str, &Value)], deadline: std::time::Duration) -> Result<bool, Error> {
    let expr = expr.to_string();
    let owned_vars: Vec<(&'static str, Value)> = vars.iter().map(|(name, value)| (*name, (*value).clone())).collect();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let borrowed: Vec<(&'static str, &Value)> = owned_vars.iter().map(|(name, value)| (*name, value)).collect();
        // If the receiver already gave up (deadline passed), sending
        // here simply fails silently -- there is no one left to tell,
        // matching `mpsc::Sender::send`'s own documented behavior for a
        // disconnected receiver.
        let _ = tx.send(eval_bool_with_vars(&expr, &borrowed));
    });
    rx.recv_timeout(deadline).unwrap_or(Err(Error::DeadlineExceeded))
}

/// Real upstream's own `messageExpression`/`auditAnnotations[].
/// valueExpression` shape: same compile-bind-execute sequence as
/// [`eval_bool_with_vars`], except the CEL expression must evaluate to a
/// `string` rather than a `bool` — real upstream's own `ValidatingAdmission
/// Policy` uses this to let a denial message be composed from the request
/// (`k8s.io/apiserver/pkg/admission/plugin/policy/validating/validator.go`'s
/// own `Validate`, fetched and read directly, is where this crate's
/// `admission::policy_validations` module borrows the real message-
/// resolution order from).
pub fn eval_string_with_vars(expr: &str, vars: &[(&'static str, &Value)]) -> Result<String, Error> {
    let program = Program::compile(expr)?;
    let mut ctx = Context::default();
    register_kubernetes_extensions(&mut ctx);
    for (name, value) in vars.iter().copied() {
        ctx.add_variable(name, value.clone()).map_err(|_| Error::Bind { name })?;
    }
    match program.execute(&ctx)? {
        CelValue::String(s) => Ok((*s).clone()),
        other => Err(Error::NotString(other)),
    }
}

/// [`eval_string_with_vars`] under the same real wall-clock deadline stand-in
/// [`eval_bool_with_vars_and_deadline`] already uses — see that function's
/// own doc comment for the real, named limitation this shares.
pub fn eval_string_with_vars_and_deadline(expr: &str, vars: &[(&'static str, &Value)], deadline: std::time::Duration) -> Result<String, Error> {
    let expr = expr.to_string();
    let owned_vars: Vec<(&'static str, Value)> = vars.iter().map(|(name, value)| (*name, (*value).clone())).collect();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let borrowed: Vec<(&'static str, &Value)> = owned_vars.iter().map(|(name, value)| (*name, value)).collect();
        let _ = tx.send(eval_string_with_vars(&expr, &borrowed));
    });
    rx.recv_timeout(deadline).unwrap_or(Err(Error::DeadlineExceeded))
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

    #[test]
    fn a_fast_expression_completes_well_within_a_generous_deadline() {
        let value = json!({"spec": {"replicas": 3}});
        let result = eval_bool_with_deadline("self.spec.replicas > 0", &value, None, std::time::Duration::from_secs(5));
        assert_eq!(result.unwrap(), true);
    }

    #[test]
    fn an_expired_deadline_is_a_real_named_error_not_a_hang_or_panic() {
        // A trivial expression like `1 + 1 == 2` races the deadline
        // rather than reliably losing to it: `mpsc::Receiver::
        // recv_timeout` returns whatever's already in the channel
        // immediately, ignoring the requested duration entirely, if the
        // spawned thread happens to finish and send before the caller
        // even reaches the `recv_timeout` call -- genuinely possible for
        // near-instant work regardless of how small the deadline is,
        // caught for real on a busy single-core CI runner (a thread
        // spawn there can itself context-switch straight into the new
        // thread before the parent resumes).
        //
        // A real CEL expression forced to do a bounded but substantial
        // amount of interpreter work -- a triple-nested `all()` over a
        // 100-element list, one million individual arithmetic
        // comparisons walked by a tree-walking interpreter -- cannot
        // finish before the parent thread reaches `recv_timeout` on any
        // real hardware, so this is deterministic instead of a race.
        let nums: Vec<i64> = (0..100).collect();
        let list = format!("{nums:?}");
        let expr = format!("{list}.all(x, {list}.all(y, {list}.all(z, x + y + z >= -3)))");
        let value = json!({});
        let result = eval_bool_with_deadline(&expr, &value, None, std::time::Duration::from_micros(1));
        assert!(matches!(result, Err(Error::DeadlineExceeded)), "expected Error::DeadlineExceeded, got {result:?}");
    }

    #[test]
    fn a_deadlined_evaluation_still_reports_a_real_compile_error_when_it_has_time_to() {
        let value = json!({});
        let result = eval_bool_with_deadline("this is not valid cel (((", &value, None, std::time::Duration::from_secs(5));
        assert!(matches!(result, Err(Error::Compile(_))), "expected Error::Compile, got {result:?}");
    }

    #[test]
    fn eval_bool_with_vars_binds_an_arbitrary_variable_set() {
        let object = json!({"name": "x"});
        let old_object = json!({"name": "y"});
        let result = eval_bool_with_vars("object.name != oldObject.name", &[("object", &object), ("oldObject", &old_object)]);
        assert_eq!(result.unwrap(), true);
    }

    #[test]
    fn eval_bool_with_vars_supports_a_real_variable_name_other_than_self_or_old_self() {
        let request = json!({"operation": "CREATE"});
        let result = eval_bool_with_vars("request.operation == 'CREATE'", &[("request", &request)]);
        assert_eq!(result.unwrap(), true);
    }

    #[test]
    fn eval_bool_backed_by_eval_bool_with_vars_is_unchanged_for_self_and_old_self() {
        let value = json!({"replicas": 3});
        let old = json!({"replicas": 2});
        assert_eq!(eval_bool("self.replicas > oldSelf.replicas", &value, Some(&old)).unwrap(), true);
    }

    #[test]
    fn eval_bool_with_vars_and_deadline_binds_the_same_arbitrary_variable_set() {
        let object = json!({"name": "x"});
        let result = eval_bool_with_vars_and_deadline("object.name == 'x'", &[("object", &object)], std::time::Duration::from_secs(5));
        assert_eq!(result.unwrap(), true);
    }

    #[test]
    fn eval_string_with_vars_returns_the_real_string_result() {
        let object = json!({"name": "widget"});
        let result = eval_string_with_vars("'must not be named ' + object.name", &[("object", &object)]);
        assert_eq!(result.unwrap(), "must not be named widget");
    }

    #[test]
    fn eval_string_with_vars_reports_a_non_string_result_as_a_real_error() {
        let object = json!({"replicas": 3});
        let result = eval_string_with_vars("object.replicas", &[("object", &object)]);
        assert!(matches!(result, Err(Error::NotString(_))), "expected Error::NotString, got {result:?}");
    }

    #[test]
    fn eval_string_with_vars_and_deadline_binds_the_same_arbitrary_variable_set() {
        let object = json!({"name": "x"});
        let result = eval_string_with_vars_and_deadline("object.name", &[("object", &object)], std::time::Duration::from_secs(5));
        assert_eq!(result.unwrap(), "x");
    }

    #[test]
    fn kubernetes_extensions_are_registered_and_reachable_from_a_real_expression() {
        // A real live round trip through the actual cel::Context, not
        // just kubernetes_lists::is_sorted's own pure unit tests --
        // proves register_kubernetes_extensions really wires the
        // function through Context::add_function and a real CEL
        // expression can actually call it by name.
        assert_eq!(eval_bool_with_vars("[1, 2, 3].isSorted()", &[]).unwrap(), true);
        assert_eq!(eval_bool_with_vars("[3, 2, 1].isSorted()", &[]).unwrap(), false);
    }

    #[test]
    fn kubernetes_extensions_are_reachable_through_the_object_variable_too() {
        let object = json!({"values": [1, 2, 3]});
        assert_eq!(eval_bool("self.values.isSorted()", &object, None).unwrap(), true);
    }
}
