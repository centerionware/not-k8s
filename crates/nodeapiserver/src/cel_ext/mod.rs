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
//! `check_rule_cost`/`check_rule_cost_with_cardinality`, the real
//! accept/reject decision for one rule against
//! [`budget::STATIC_ESTIMATED_COST_LIMIT`] (real upstream's own
//! `StaticEstimatedCostLimit`), including the propagated
//! `MaxCardinality` multiplier for rules nested below repeating
//! array/map schemas. **Now wired into a
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
//! (`PerCallLimit`'s real ~0.1s). The same evaluator now enforces one
//! shared ~1s wall-clock `RuntimeCELCostBudget` window per object and
//! stops walking further schema rules once it is exhausted. Wired into
//! `server::rest::create`/`update`/`patch_persist`'s existing CRD branches
//! — a real custom resource that fails its own declared rule now gets a
//! real `422` with the rule's own declared `message`. The remaining
//! limitation is interpreter-level cost/fuel accounting: the `cel` crate
//! exposes no interruption hook, so a timed-out worker thread may finish
//! in the background even though the request is bounded and the request
//! concurrency gate limits how many can be started at once. See
//! `docs/APISERVER.md`'s own
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
//! Group K's `x-kubernetes-validations` path now uses the static check and
//! the shared request-side wall-clock budget in
//! `apiextensions::cel_evaluate`; callers must use that budgeted path rather
//! than treating the raw deadline helper as an unbounded request primitive.
//! `type_check` supplies the schema-aware declaration phase for those CRD
//! rules: it resolves `self`/`oldSelf`, checks exposed fields and obvious
//! overloads (including the opaque Kubernetes extension values), and
//! enforces a boolean result at CRD acceptance while leaving dynamic schema
//! portions permissive. Rules opting into `optionalOldSelf` receive the
//! native CEL optional form at runtime; ordinary transition rules are skipped
//! when there is no prior value to compare.
//! The remaining difference from upstream is that the `cel` crate exposes
//! no interpreter-level fuel or interruption hook, so timed-out evaluation
//! threads cannot be forcibly reclaimed.
//!
//! Named `cel_ext`, not `cel` — see the module-map note in `lib.rs` for why
//! (this crate also depends on the external `cel` crate).
//!
//! **Group K point 6 started**: `kubernetes_lists` is real upstream's own
//! `kubernetes.lists` library (`k8s.io/apiserver/pkg/cel/library/
//! lists.go`, fetched and read directly) — every function it declares
//! (`isSorted`/`min`/`max`/`indexOf`/`lastIndexOf`/`sum`/`includes`) is
//! now landed. `kubernetes_quantity` is real upstream's own `kubernetes.
//! quantity` library (`.../library/quantity.go`), including its opaque
//! `Quantity` value, constructor, scalar conversions, arithmetic, and
//! comparison member functions.
//! `kubernetes_ip` is the corresponding strict IP parser and classifier
//! surface from upstream's `kubernetes.net.ip` library.
//! `kubernetes_cidr` builds on that value for upstream's `kubernetes.net.cidr`
//! parser, containment, masking, and prefix-length helpers.
//! `kubernetes_url` provides the upstream `kubernetes.urls` parser and URL
//! component/query accessors.
//! `kubernetes_semver` provides the upstream `kubernetes.Semver` parser,
//! normalization, comparison, and component accessors.
//! `kubernetes_regex` provides upstream's `find` and `findAll` substring
//! helpers, including the optional match limit.
//! `kubernetes_format` provides named DNS, label, URI, UUID, byte, date, and
//! datetime validators through native CEL optional values.
//! `register_kubernetes_extensions` wires every one of them onto
//! every `Context` this module builds via `cel::Context::add_function`
//! (`cel-rust`'s own real custom-function registration API, confirmed
//! against that crate's own `example/src/functions.rs`/`cel/src/
//! functions.rs`, which the published docs don't render).
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
pub mod authorizer;
pub mod kubernetes_cidr;
pub mod kubernetes_format;
pub mod kubernetes_ip;
pub mod kubernetes_lists;
pub mod kubernetes_quantity;
pub mod kubernetes_regex;
pub mod kubernetes_semver;
pub mod kubernetes_url;
pub mod path;
pub mod type_check;
pub mod typed_mutation;

use cel::extractors::This;
use cel::objects::OptionalValue;
use cel::{Context, FunctionContext, Program, Value as CelValue};
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
    #[error("the CEL expression result could not be converted to JSON: {0}")]
    Serialize(String),
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

/// Evaluate a rule whose `oldSelf` variable is the Kubernetes optional type.
/// `optionalOldSelf: true` keeps the variable defined on CREATE, where it is
/// `optional.none()`, while UPDATE binds `optional.of(oldSelf)`.
pub fn eval_bool_with_optional_old_self(
    expr: &str,
    self_value: &Value,
    old_self_value: Option<&Value>,
) -> Result<bool, Error> {
    let old_self = optional_old_self_value(old_self_value)?;
    eval_bool_with_vars_and_cel_vars(expr, &[("self", self_value)], &[("oldSelf", old_self)])
}

fn optional_old_self_value(old_self_value: Option<&Value>) -> Result<CelValue, Error> {
    let optional = match old_self_value {
        Some(value) => {
            let value = cel::to_value(value.clone()).map_err(|error| Error::Serialize(error.to_string()))?;
            OptionalValue::of(value)
        }
        None => OptionalValue::none(),
    };
    Ok(CelValue::Opaque(std::sync::Arc::new(optional)))
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
/// Registers this crate's own real Kubernetes CEL extension functions onto a
/// fresh [`Context`] — called by every real entry point below so a rule can
/// use them regardless of which variable-naming convention it's
/// evaluated through.
pub(crate) fn register_kubernetes_extensions(ctx: &mut Context) {
    ctx.add_function("isSorted", kubernetes_lists::is_sorted_binding);
    ctx.add_function("min", kubernetes_lists::min_binding);
    ctx.add_function("max", kubernetes_lists::max_binding);
    ctx.add_function("indexOf", kubernetes_lists::index_of_binding);
    ctx.add_function("lastIndexOf", kubernetes_lists::last_index_of_binding);
    ctx.add_function("sum", kubernetes_lists::sum_binding);
    ctx.add_function("includes", kubernetes_lists::includes_binding);
    ctx.add_function("isQuantity", kubernetes_quantity::is_quantity_binding);
    ctx.add_function("quantity", kubernetes_quantity::quantity_binding);
    ctx.add_function("isInteger", kubernetes_quantity::is_integer_binding);
    ctx.add_function("asInteger", kubernetes_quantity::as_integer_binding);
    ctx.add_function("asApproximateFloat", kubernetes_quantity::as_approximate_float_binding);
    ctx.add_function("sign", kubernetes_quantity::sign_binding);
    ctx.add_function("add", kubernetes_quantity::add_binding);
    ctx.add_function("sub", kubernetes_quantity::sub_binding);
    ctx.add_function("isLessThan", kubernetes_quantity::is_less_than_binding);
    ctx.add_function("isGreaterThan", kubernetes_quantity::is_greater_than_binding);
    ctx.add_function("compareTo", kubernetes_quantity::compare_to_binding);
    ctx.add_function("ip.isCanonical", kubernetes_ip::is_canonical_binding);
    ctx.add_function("isIP", kubernetes_ip::is_ip_binding);
    ctx.add_function("family", kubernetes_ip::family_binding);
    ctx.add_function("isUnspecified", kubernetes_ip::is_unspecified_binding);
    ctx.add_function("isLoopback", kubernetes_ip::is_loopback_binding);
    ctx.add_function("isLinkLocalMulticast", kubernetes_ip::is_link_local_multicast_binding);
    ctx.add_function("isLinkLocalUnicast", kubernetes_ip::is_link_local_unicast_binding);
    ctx.add_function("isGlobalUnicast", kubernetes_ip::is_global_unicast_binding);
    ctx.add_function("cidr", kubernetes_cidr::cidr_binding);
    ctx.add_function("isCIDR", kubernetes_cidr::is_cidr_binding);
    ctx.add_function("containsIP", kubernetes_cidr::contains_ip_binding);
    ctx.add_function("containsCIDR", kubernetes_cidr::contains_cidr_binding);
    ctx.add_function("ip", kubernetes_cidr::ip_binding);
    ctx.add_function("prefixLength", kubernetes_cidr::prefix_length_binding);
    ctx.add_function("masked", kubernetes_cidr::masked_binding);
    ctx.add_function("url", kubernetes_url::url_binding);
    ctx.add_function("isURL", kubernetes_url::is_url_binding);
    ctx.add_function("getScheme", kubernetes_url::get_scheme_binding);
    ctx.add_function("getHost", kubernetes_url::get_host_binding);
    ctx.add_function("getHostname", kubernetes_url::get_hostname_binding);
    ctx.add_function("getPort", kubernetes_url::get_port_binding);
    ctx.add_function("getEscapedPath", kubernetes_url::get_escaped_path_binding);
    ctx.add_function("getQuery", kubernetes_url::get_query_binding);
    ctx.add_function("semver", kubernetes_semver::semver_binding);
    ctx.add_function("isSemver", kubernetes_semver::is_semver_binding);
    ctx.add_function("isGreaterThan", kubernetes_semver::is_greater_than_binding);
    ctx.add_function("isLessThan", kubernetes_semver::is_less_than_binding);
    ctx.add_function("compareTo", kubernetes_semver::compare_to_binding);
    ctx.add_function("major", kubernetes_semver::major_binding);
    ctx.add_function("minor", kubernetes_semver::minor_binding);
    ctx.add_function("patch", kubernetes_semver::patch_binding);
    ctx.add_function("find", kubernetes_regex::find_binding);
    ctx.add_function("findAll", kubernetes_regex::find_all_binding);
    ctx.add_function("format.named", kubernetes_format::named_binding);
    ctx.add_function("validate", kubernetes_format::validate_binding);
    ctx.add_function("format.dns1123Label", kubernetes_format::dns1123_label_binding);
    ctx.add_function("format.dns1123Subdomain", kubernetes_format::dns1123_subdomain_binding);
    ctx.add_function("format.dns1035Label", kubernetes_format::dns1035_label_binding);
    ctx.add_function("format.qualifiedName", kubernetes_format::qualified_name_binding);
    ctx.add_function("format.dns1123LabelPrefix", kubernetes_format::dns1123_label_prefix_binding);
    ctx.add_function("format.dns1123SubdomainPrefix", kubernetes_format::dns1123_subdomain_prefix_binding);
    ctx.add_function("format.dns1035LabelPrefix", kubernetes_format::dns1035_label_prefix_binding);
    ctx.add_function("format.labelValue", kubernetes_format::label_value_binding);
    ctx.add_function("format.uri", kubernetes_format::uri_binding);
    ctx.add_function("format.uuid", kubernetes_format::uuid_binding);
    ctx.add_function("format.byte", kubernetes_format::byte_binding);
    ctx.add_function("format.date", kubernetes_format::date_binding);
    ctx.add_function("format.datetime", kubernetes_format::datetime_binding);
    ctx.add_function("string", string_binding);
    authorizer::register(ctx);
}

fn string_binding(ftx: &FunctionContext, This(value): This<CelValue>) -> Result<CelValue, cel::ExecutionError> {
    if let Some(value) = kubernetes_url::string_value(&value) {
        return Ok(CelValue::String(std::sync::Arc::new(value)));
    }
    if let Some(value) = kubernetes_cidr::string_value(&value) {
        return Ok(CelValue::String(std::sync::Arc::new(value)));
    }
    if let Some(value) = kubernetes_semver::string_value(&value) {
        return Ok(CelValue::String(std::sync::Arc::new(value)));
    }
    kubernetes_ip::string_binding(ftx, This(value))
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

/// Evaluate a boolean expression with ordinary JSON variables plus one or
/// more native CEL values, such as the opaque Kubernetes `authorizer`.
pub fn eval_bool_with_vars_and_cel_vars(expr: &str, vars: &[(&'static str, &Value)], cel_vars: &[(&'static str, CelValue)]) -> Result<bool, Error> {
    let program = Program::compile(expr)?;
    let mut ctx = Context::default();
    register_kubernetes_extensions(&mut ctx);
    for (name, value) in vars.iter().copied() {
        ctx.add_variable(name, value.clone()).map_err(|_| Error::Bind { name })?;
    }
    for (name, value) in cel_vars.iter().cloned() {
        ctx.add_variable(name, value).map_err(|_| Error::Bind { name })?;
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

/// Deadline-bounded variant of [`eval_bool_with_optional_old_self`], used by
/// the CRD runtime evaluator so optional-old-self rules receive the same
/// request-side CEL deadline as ordinary rules.
pub fn eval_bool_with_optional_old_self_and_deadline(
    expr: &str,
    self_value: &Value,
    old_self_value: Option<&Value>,
    deadline: std::time::Duration,
) -> Result<bool, Error> {
    let old_self = optional_old_self_value(old_self_value)?;
    eval_bool_with_vars_and_cel_vars_and_deadline(
        expr,
        &[("self", self_value)],
        &[("oldSelf", old_self)],
        deadline,
    )
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

/// Evaluate a boolean expression with JSON and native CEL variables under
/// the same request-side deadline as [`eval_bool_with_vars_and_deadline`].
pub fn eval_bool_with_vars_and_cel_vars_and_deadline(expr: &str, vars: &[(&'static str, &Value)], cel_vars: &[(&'static str, CelValue)], deadline: std::time::Duration) -> Result<bool, Error> {
    let expr = expr.to_string();
    let owned_vars: Vec<(&'static str, Value)> = vars.iter().map(|(name, value)| (*name, (*value).clone())).collect();
    let owned_cel_vars: Vec<(&'static str, CelValue)> = cel_vars.iter().map(|(name, value)| (*name, value.clone())).collect();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let borrowed: Vec<(&'static str, &Value)> = owned_vars.iter().map(|(name, value)| (*name, value)).collect();
        let _ = tx.send(eval_bool_with_vars_and_cel_vars(&expr, &borrowed, &owned_cel_vars));
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

/// Evaluate a CEL expression whose result is a JSON-shaped value. This is
/// the common representation used by admission policies: the CEL runtime
/// evaluates the policy's typed value and the admission layer then applies
/// the resulting JSON Patch or apply configuration to the request object.
pub fn eval_json_with_vars(expr: &str, vars: &[(&'static str, &Value)]) -> Result<Value, Error> {
    let program = Program::compile(expr)?;
    let mut ctx = Context::default();
    register_kubernetes_extensions(&mut ctx);
    for (name, value) in vars.iter().copied() {
        ctx.add_variable(name, value.clone()).map_err(|_| Error::Bind { name })?;
    }
    program
        .execute(&ctx)?
        .json()
        .map_err(|error| Error::Serialize(error.to_string()))
}

/// Evaluate a JSON-shaped expression with ordinary JSON variables plus
/// native CEL values such as the opaque Kubernetes `authorizer`.
pub fn eval_json_with_vars_and_cel_vars(expr: &str, vars: &[(&'static str, &Value)], cel_vars: &[(&'static str, CelValue)]) -> Result<Value, Error> {
    let program = Program::compile(expr)?;
    let mut ctx = Context::default();
    register_kubernetes_extensions(&mut ctx);
    for (name, value) in vars.iter().copied() {
        ctx.add_variable(name, value.clone()).map_err(|_| Error::Bind { name })?;
    }
    for (name, value) in cel_vars.iter().cloned() {
        ctx.add_variable(name, value).map_err(|_| Error::Bind { name })?;
    }
    program
        .execute(&ctx)?
        .json()
        .map_err(|error| Error::Serialize(error.to_string()))
}

/// Evaluate [`eval_json_with_vars`] under the same wall-clock deadline used
/// by the boolean and string admission CEL helpers.
pub fn eval_json_with_vars_and_deadline(expr: &str, vars: &[(&'static str, &Value)], deadline: std::time::Duration) -> Result<Value, Error> {
    let expr = expr.to_string();
    let owned_vars: Vec<(&'static str, Value)> = vars.iter().map(|(name, value)| (*name, (*value).clone())).collect();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let borrowed: Vec<(&'static str, &Value)> = owned_vars.iter().map(|(name, value)| (*name, value)).collect();
        let _ = tx.send(eval_json_with_vars(&expr, &borrowed));
    });
    rx.recv_timeout(deadline).unwrap_or(Err(Error::DeadlineExceeded))
}

/// Evaluate a JSON-shaped expression with JSON and native CEL variables
/// under the same request-side deadline used by the other CEL helpers.
pub fn eval_json_with_vars_and_cel_vars_and_deadline(expr: &str, vars: &[(&'static str, &Value)], cel_vars: &[(&'static str, CelValue)], deadline: std::time::Duration) -> Result<Value, Error> {
    let expr = expr.to_string();
    let owned_vars: Vec<(&'static str, Value)> = vars.iter().map(|(name, value)| (*name, (*value).clone())).collect();
    let owned_cel_vars: Vec<(&'static str, CelValue)> = cel_vars.iter().map(|(name, value)| (*name, value.clone())).collect();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let borrowed: Vec<(&'static str, &Value)> = owned_vars.iter().map(|(name, value)| (*name, value)).collect();
        let _ = tx.send(eval_json_with_vars_and_cel_vars(&expr, &borrowed, &owned_cel_vars));
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
    fn eval_json_with_vars_round_trips_a_mutation_document() {
        let value = eval_json_with_vars(
            r#"[{"op": "add", "path": "/metadata/labels/managed", "value": "true"}]"#,
            &[],
        )
        .unwrap();
        assert_eq!(value[0]["op"], "add");
        assert_eq!(value[0]["path"], "/metadata/labels/managed");
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
        assert_eq!(eval_bool_with_vars("[3, 1, 2].min() == 1", &[]).unwrap(), true);
        assert_eq!(eval_bool_with_vars("[3, 1, 2].max() == 3", &[]).unwrap(), true);
        assert_eq!(eval_bool_with_vars("[1, 2, 2, 3].indexOf(2) == 1", &[]).unwrap(), true);
        assert_eq!(eval_bool_with_vars("[1, 2, 2, 3].lastIndexOf(2) == 2", &[]).unwrap(), true);
        assert_eq!(eval_bool_with_vars("[1, 2, 3].sum() == 6", &[]).unwrap(), true);
        assert_eq!(eval_bool_with_vars("[1, 2, 3].includes(2)", &[]).unwrap(), true);
        assert_eq!(eval_bool_with_vars("'model-a'.includes('model-a')", &[]).unwrap(), true);
        assert_eq!(eval_bool_with_vars("isQuantity('1.5G')", &[]).unwrap(), true);
        assert_eq!(eval_bool_with_vars("isQuantity('Three')", &[]).unwrap(), false);
        assert_eq!(eval_bool_with_vars("quantity('50k').asInteger() == 50000", &[]).unwrap(), true);
        assert_eq!(eval_bool_with_vars("quantity('0.5').isInteger() == false", &[]).unwrap(), true);
        assert_eq!(eval_bool_with_vars("quantity('50k').sign() == 1", &[]).unwrap(), true);
        assert_eq!(eval_bool_with_vars("quantity('50k').add(20).sub(quantity('20')).compareTo(quantity('50k')) == 0", &[]).unwrap(), true);
        assert_eq!(eval_bool_with_vars("quantity('50Mi').isGreaterThan(quantity('50M'))", &[]).unwrap(), true);
        assert_eq!(eval_bool_with_vars("quantity('50M').isLessThan(quantity('100M'))", &[]).unwrap(), true);
        assert_eq!(eval_bool_with_vars("quantity('50k').asApproximateFloat() == 50000.0", &[]).unwrap(), true);
        assert_eq!(eval_bool_with_vars("isIP('127.0.0.1')", &[]).unwrap(), true);
        assert_eq!(eval_bool_with_vars("isIP('::ffff:192.0.2.1')", &[]).unwrap(), false);
        assert_eq!(eval_bool_with_vars("ip('127.0.0.1').family() == 4", &[]).unwrap(), true);
        assert_eq!(eval_bool_with_vars("ip('::1').family() == 6", &[]).unwrap(), true);
        assert_eq!(eval_bool_with_vars("ip('0.0.0.0').isUnspecified()", &[]).unwrap(), true);
        assert_eq!(eval_bool_with_vars("ip('224.0.0.1').isLinkLocalMulticast()", &[]).unwrap(), true);
        assert_eq!(eval_bool_with_vars("ip('169.254.169.254').isLinkLocalUnicast()", &[]).unwrap(), true);
        assert_eq!(eval_bool_with_vars("ip('192.168.0.1').isGlobalUnicast()", &[]).unwrap(), true);
        assert_eq!(eval_bool_with_vars("ip('2001:DB8::ABCD').string() == '2001:db8::abcd'", &[]).unwrap(), true);
        assert_eq!(eval_bool_with_vars("ip.isCanonical('2001:db8::abcd')", &[]).unwrap(), true);
        assert_eq!(eval_bool_with_vars("ip.isCanonical('2001:DB8::ABCD')", &[]).unwrap(), false);
        assert_eq!(eval_bool_with_vars("isCIDR('192.168.0.0/24')", &[]).unwrap(), true);
        assert_eq!(eval_bool_with_vars("isCIDR('192.168.0.0/33')", &[]).unwrap(), false);
        assert_eq!(eval_bool_with_vars("cidr('192.168.0.1/24').containsIP('192.168.0.10')", &[]).unwrap(), true);
        assert_eq!(eval_bool_with_vars("cidr('192.168.0.0/24').containsIP(ip('192.168.0.10'))", &[]).unwrap(), true);
        assert_eq!(eval_bool_with_vars("cidr('192.168.0.0/16').containsCIDR('192.168.10.0/24')", &[]).unwrap(), true);
        assert_eq!(eval_bool_with_vars("cidr('192.168.0.0/24').containsCIDR('192.168.1.0/24')", &[]).unwrap(), false);
        assert_eq!(eval_bool_with_vars("cidr('192.168.0.1/24').masked().string() == '192.168.0.0/24'", &[]).unwrap(), true);
        assert_eq!(eval_bool_with_vars("cidr('192.168.0.1/24').prefixLength() == 24", &[]).unwrap(), true);
        assert_eq!(eval_bool_with_vars("cidr('::1/128').ip().family() == 6", &[]).unwrap(), true);
        assert_eq!(eval_bool_with_vars("string(cidr('192.168.0.1/24')) == '192.168.0.1/24'", &[]).unwrap(), true);
        assert_eq!(eval_bool_with_vars("isURL('https://example.com/path')", &[]).unwrap(), true);
        assert_eq!(eval_bool_with_vars("isURL('/relative/path')", &[]).unwrap(), false);
        assert_eq!(eval_bool_with_vars("url('https://example.com:8443/path%20with%20spaces?k=a&k=b').getScheme() == 'https'", &[]).unwrap(), true);
        assert_eq!(eval_bool_with_vars("url('https://example.com:8443/path%20with%20spaces?k=a&k=b').getHost() == 'example.com:8443'", &[]).unwrap(), true);
        assert_eq!(eval_bool_with_vars("url('https://example.com:8443/path%20with%20spaces?k=a&k=b').getHostname() == 'example.com'", &[]).unwrap(), true);
        assert_eq!(eval_bool_with_vars("url('https://example.com:8443/path%20with%20spaces?k=a&k=b').getPort() == '8443'", &[]).unwrap(), true);
        assert_eq!(eval_bool_with_vars("url('https://example.com:8443/path%20with%20spaces?k=a&k=b').getEscapedPath() == '/path%20with%20spaces'", &[]).unwrap(), true);
        assert_eq!(eval_bool_with_vars("url('https://example.com/path?k=a&k=b').getQuery()['k'][1] == 'b'", &[]).unwrap(), true);
        assert_eq!(eval_bool_with_vars("string(url('https://example.com/path')) == 'https://example.com/path'", &[]).unwrap(), true);
        assert_eq!(eval_bool_with_vars("isSemver('1.2.3')", &[]).unwrap(), true);
        assert_eq!(eval_bool_with_vars("isSemver('v1.2.3')", &[]).unwrap(), false);
        assert_eq!(eval_bool_with_vars("isSemver('v01.2', true)", &[]).unwrap(), true);
        assert_eq!(eval_bool_with_vars("semver('1.2.3').major() == 1", &[]).unwrap(), true);
        assert_eq!(eval_bool_with_vars("semver('1.2.3').minor() == 2", &[]).unwrap(), true);
        assert_eq!(eval_bool_with_vars("semver('1.2.3').patch() == 3", &[]).unwrap(), true);
        assert_eq!(eval_bool_with_vars("semver('1.2.3').isLessThan(semver('2.0.0'))", &[]).unwrap(), true);
        assert_eq!(eval_bool_with_vars("semver('2.0.0').isGreaterThan(semver('1.2.3'))", &[]).unwrap(), true);
        assert_eq!(eval_bool_with_vars("semver('1.2.3').compareTo(semver('1.2.3')) == 0", &[]).unwrap(), true);
        assert_eq!(eval_bool_with_vars("string(semver('1.2.3')) == '1.2.3'", &[]).unwrap(), true);
        assert_eq!(eval_bool_with_vars("'abc 123 def 456'.find('[0-9]+') == '123'", &[]).unwrap(), true);
        assert_eq!(eval_bool_with_vars("'123 abc 456'.findAll('[0-9]+')[1] == '456'", &[]).unwrap(), true);
        assert_eq!(eval_bool_with_vars("'123 abc 456'.findAll('[0-9]+', 1).size() == 1", &[]).unwrap(), true);
        assert_eq!(eval_bool_with_vars("format.dns1123Label().validate('valid-name').hasValue() == false", &[]).unwrap(), true);
        assert_eq!(eval_bool_with_vars("format.dns1123Label().validate('-invalid').hasValue()", &[]).unwrap(), true);
        assert_eq!(eval_bool_with_vars("format.named('uuid').value().validate('123e4567-e89b-12d3-a456-426614174000').hasValue() == false", &[]).unwrap(), true);
        assert_eq!(eval_bool_with_vars("format.date().validate('2021-01-01').hasValue() == false", &[]).unwrap(), true);
    }

    #[test]
    fn min_on_a_real_empty_list_is_a_real_execution_error_not_a_panic() {
        assert!(eval_bool_with_vars("[].min() == 0", &[]).is_err());
    }

    #[test]
    fn kubernetes_extensions_are_reachable_through_the_object_variable_too() {
        let object = json!({"values": [1, 2, 3]});
        assert_eq!(eval_bool("self.values.isSorted()", &object, None).unwrap(), true);
    }
}
