//! Kubernetes' `authorizer` CEL library for admission expressions.
//!
//! The upstream library exposes a small fluent object graph rather than a
//! JSON value: `authorizer.group("apps").resource("deployments")`
//! produces a check which can be finished with `.namespace()`, `.name()` and
//! `.check("get").allowed()`. The `cel` crate supports opaque values and
//! receiver functions, so this module preserves that shape while keeping the
//! actual authorization decision in the existing RBAC implementation.
//!
//! CEL execution is synchronous. Callers therefore construct an
//! [`AuthorizationContext`] from a request-local RBAC snapshot before
//! evaluating a policy. The receiver functions below only manipulate that
//! immutable context and never perform storage I/O.

use crate::authz::{rbac, resolve};
use cel::extractors::This;
use cel::objects::{KeyRef, Map, Opaque};
use cel::{Context, ExecutionError, FunctionContext, Value};
use std::collections::HashMap;
use std::sync::Arc;

const AUTHORITATIVE_TYPE: &str = "kubernetes.authorization.Authorizer";
const GROUP_TYPE: &str = "kubernetes.authorization.GroupCheck";
const RESOURCE_TYPE: &str = "kubernetes.authorization.ResourceCheck";
const PATH_TYPE: &str = "kubernetes.authorization.PathCheck";
const DECISION_TYPE: &str = "kubernetes.authorization.Decision";
const AUTHORITATIVE_FIELD: &str = "__notk8s_authorizer";

/// The resource fields used by an admission request's
/// `authorizer.requestResource` value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestResource {
    pub group: String,
    pub resource: String,
    pub subresource: String,
    pub namespace: String,
    pub name: String,
    pub path: String,
}

/// Immutable authorization inputs shared by all values created while one
/// policy is evaluated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthorizationContext {
    snapshot: Option<Arc<resolve::Snapshot>>,
    fallback_rules: Arc<Vec<rbac::PolicyRule>>,
    user_name: String,
    user_groups: Vec<String>,
    request: RequestResource,
}

impl AuthorizationContext {
    /// Build a context backed by the request-local RBAC snapshot.
    pub fn from_snapshot(snapshot: Arc<resolve::Snapshot>, user_name: String, user_groups: Vec<String>, request: RequestResource) -> Self {
        Self { snapshot: Some(snapshot), fallback_rules: Arc::new(Vec::new()), user_name, user_groups, request }
    }

    /// Build a context for pure tests or callers that already resolved the
    /// rules. Production admission uses [`Self::from_snapshot`].
    pub fn from_rules(rules: Vec<rbac::PolicyRule>, user_name: String, user_groups: Vec<String>, request: RequestResource) -> Self {
        Self { snapshot: None, fallback_rules: Arc::new(rules), user_name, user_groups, request }
    }

    fn with_principal(&self, user_name: String, user_groups: Vec<String>) -> Self {
        let fallback_rules = if self.user_name == user_name { self.fallback_rules.clone() } else { Arc::new(Vec::new()) };
        Self { snapshot: self.snapshot.clone(), fallback_rules, user_name, user_groups, request: self.request.clone() }
    }

    fn rules_for(&self, namespace: &str) -> Vec<rbac::PolicyRule> {
        self.snapshot
            .as_ref()
            .map(|snapshot| snapshot.rules_for(&self.user_name, &self.user_groups, namespace).rules)
            .unwrap_or_else(|| self.fallback_rules.as_ref().clone())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum AuthorizerValue {
    Authorizer(Arc<AuthorizationContext>),
    Group { context: Arc<AuthorizationContext>, group: String },
    Resource { context: Arc<AuthorizationContext>, group: String, resource: String, subresource: String, namespace: String, name: String },
    Path { context: Arc<AuthorizationContext>, path: String },
    Decision { allowed: bool, reason: String, error: Option<String> },
}

impl Opaque for AuthorizerValue {
    fn runtime_type_name(&self) -> &str {
        match self {
            Self::Authorizer(_) => AUTHORITATIVE_TYPE,
            Self::Group { .. } => GROUP_TYPE,
            Self::Resource { .. } => RESOURCE_TYPE,
            Self::Path { .. } => PATH_TYPE,
            Self::Decision { .. } => DECISION_TYPE,
        }
    }
}

fn opaque(value: AuthorizerValue) -> Value {
    Value::Opaque(Arc::new(value))
}

fn opaque_ref(value: &Value) -> Option<&AuthorizerValue> {
    match value {
        Value::Opaque(value) => value.downcast_ref::<AuthorizerValue>(),
        _ => None,
    }
}

fn function_error(ftx: &FunctionContext, message: impl Into<String>) -> ExecutionError {
    ftx.error(message.into())
}

fn authorizer_context(value: &Value) -> Option<Arc<AuthorizationContext>> {
    if let Some(AuthorizerValue::Authorizer(context)) = opaque_ref(value) {
        return Some(context.clone());
    }
    let Value::Map(map) = value else { return None };
    let field = map.get(&KeyRef::String(AUTHORITATIVE_FIELD))?;
    match opaque_ref(field) {
        Some(AuthorizerValue::Authorizer(context)) => Some(context.clone()),
        _ => None,
    }
}

fn authorizer_map(context: Arc<AuthorizationContext>) -> Value {
    let request = context.request.clone();
    let request_resource = opaque(AuthorizerValue::Resource {
        context: context.clone(),
        group: request.group,
        resource: request.resource,
        subresource: request.subresource,
        namespace: request.namespace,
        name: request.name,
    });
    Value::Map(Map::from(HashMap::from([
        ("requestResource".to_string(), request_resource),
        (AUTHORITATIVE_FIELD.to_string(), opaque(AuthorizerValue::Authorizer(context))),
    ])))
}

/// Construct the CEL value bound to the `authorizer` variable.
pub fn value(context: AuthorizationContext) -> Value {
    authorizer_map(Arc::new(context))
}

fn authorizer_group(ftx: &FunctionContext, This(value): This<Value>, group: Arc<String>) -> Result<Value, ExecutionError> {
    let Some(context) = authorizer_context(&value) else {
        return Err(function_error(ftx, "group() requires an authorizer value"));
    };
    Ok(opaque(AuthorizerValue::Group { context, group: group.as_str().to_string() }))
}

fn authorizer_path(ftx: &FunctionContext, This(value): This<Value>, path: Arc<String>) -> Result<Value, ExecutionError> {
    let Some(context) = authorizer_context(&value) else {
        return Err(function_error(ftx, "path() requires an authorizer value"));
    };
    if path.trim().is_empty() {
        return Err(function_error(ftx, "path must not be empty"));
    }
    Ok(opaque(AuthorizerValue::Path { context, path: path.as_str().to_string() }))
}

fn authorizer_service_account(ftx: &FunctionContext, This(value): This<Value>, namespace: Arc<String>, name: Arc<String>) -> Result<Value, ExecutionError> {
    let Some(context) = authorizer_context(&value) else {
        return Err(function_error(ftx, "serviceAccount() requires an authorizer value"));
    };
    if !valid_dns_label(&namespace) || !valid_dns_subdomain(&name) {
        return Err(function_error(ftx, "invalid service account namespace or name"));
    }
    let user_name = crate::authz::subject::service_account_username(&namespace, &name);
    let user_groups = vec![
        "system:serviceaccounts".to_string(),
        format!("system:serviceaccounts:{namespace}"),
        "system:authenticated".to_string(),
    ];
    Ok(authorizer_map(Arc::new(context.with_principal(user_name, user_groups))))
}

fn group_resource(ftx: &FunctionContext, This(value): This<Value>, resource: Arc<String>) -> Result<Value, ExecutionError> {
    let Some(AuthorizerValue::Group { context, group }) = opaque_ref(&value) else {
        return Err(function_error(ftx, "resource() requires a group check"));
    };
    if resource.trim().is_empty() {
        return Err(function_error(ftx, "resource must not be empty"));
    }
    Ok(opaque(AuthorizerValue::Resource {
        context: context.clone(),
        group: group.clone(),
        resource: resource.as_str().to_string(),
        subresource: String::new(),
        namespace: String::new(),
        name: String::new(),
    }))
}

fn resource_subresource(ftx: &FunctionContext, This(value): This<Value>, subresource: Arc<String>) -> Result<Value, ExecutionError> {
    let Some(AuthorizerValue::Resource { context, group, resource, namespace, name, .. }) = opaque_ref(&value) else {
        return Err(function_error(ftx, "subresource() requires a resource check"));
    };
    Ok(opaque(AuthorizerValue::Resource {
        context: context.clone(),
        group: group.clone(),
        resource: resource.clone(),
        subresource: subresource.as_str().to_string(),
        namespace: namespace.clone(),
        name: name.clone(),
    }))
}

fn resource_namespace(ftx: &FunctionContext, This(value): This<Value>, namespace: Arc<String>) -> Result<Value, ExecutionError> {
    let Some(AuthorizerValue::Resource { context, group, resource, subresource, name, .. }) = opaque_ref(&value) else {
        return Err(function_error(ftx, "namespace() requires a resource check"));
    };
    Ok(opaque(AuthorizerValue::Resource {
        context: context.clone(),
        group: group.clone(),
        resource: resource.clone(),
        subresource: subresource.clone(),
        namespace: namespace.as_str().to_string(),
        name: name.clone(),
    }))
}

fn resource_name(ftx: &FunctionContext, This(value): This<Value>, name: Arc<String>) -> Result<Value, ExecutionError> {
    let Some(AuthorizerValue::Resource { context, group, resource, subresource, namespace, .. }) = opaque_ref(&value) else {
        return Err(function_error(ftx, "name() requires a resource check"));
    };
    Ok(opaque(AuthorizerValue::Resource {
        context: context.clone(),
        group: group.clone(),
        resource: resource.clone(),
        subresource: subresource.clone(),
        namespace: namespace.clone(),
        name: name.as_str().to_string(),
    }))
}

fn check(ftx: &FunctionContext, This(value): This<Value>, verb: Arc<String>) -> Result<Value, ExecutionError> {
    let decision = match opaque_ref(&value) {
        Some(AuthorizerValue::Path { context, path }) => {
            let rules = context.rules_for("");
            let attrs = rbac::RequestAttributes { is_resource_request: false, verb: verb.as_str(), path, ..Default::default() };
            rbac::rules_allow(&attrs, &rules)
        }
        Some(AuthorizerValue::Resource { context, group, resource, subresource, namespace, name }) => {
            let rules = context.rules_for(namespace);
            let attrs = rbac::RequestAttributes { is_resource_request: true, verb: verb.as_str(), api_group: group, resource, subresource, name, ..Default::default() };
            rbac::rules_allow(&attrs, &rules)
        }
        _ => return Err(function_error(ftx, "check() requires a path or resource check")),
    };
    Ok(opaque(AuthorizerValue::Decision { allowed: decision, reason: if decision { "RBAC allowed" } else { "RBAC denied" }.to_string(), error: None }))
}

fn decision_allowed(ftx: &FunctionContext, This(value): This<Value>) -> Result<bool, ExecutionError> {
    let Some(AuthorizerValue::Decision { allowed, .. }) = opaque_ref(&value) else {
        return Err(function_error(ftx, "allowed() requires an authorization decision"));
    };
    Ok(*allowed)
}

fn decision_errored(ftx: &FunctionContext, This(value): This<Value>) -> Result<bool, ExecutionError> {
    let Some(AuthorizerValue::Decision { error, .. }) = opaque_ref(&value) else {
        return Err(function_error(ftx, "errored() requires an authorization decision"));
    };
    Ok(error.is_some())
}

fn decision_error(ftx: &FunctionContext, This(value): This<Value>) -> Result<Value, ExecutionError> {
    let Some(AuthorizerValue::Decision { error, .. }) = opaque_ref(&value) else {
        return Err(function_error(ftx, "error() requires an authorization decision"));
    };
    Ok(Value::String(Arc::new(error.clone().unwrap_or_default())))
}

fn decision_reason(ftx: &FunctionContext, This(value): This<Value>) -> Result<Value, ExecutionError> {
    let Some(AuthorizerValue::Decision { reason, .. }) = opaque_ref(&value) else {
        return Err(function_error(ftx, "reason() requires an authorization decision"));
    };
    Ok(Value::String(Arc::new(reason.clone())))
}

/// Register the receiver functions used by the Kubernetes Authz CEL library.
pub fn register(ctx: &mut Context) {
    ctx.add_function("group", authorizer_group);
    ctx.add_function("path", authorizer_path);
    ctx.add_function("serviceAccount", authorizer_service_account);
    ctx.add_function("resource", group_resource);
    ctx.add_function("subresource", resource_subresource);
    ctx.add_function("namespace", resource_namespace);
    ctx.add_function("name", resource_name);
    ctx.add_function("check", check);
    ctx.add_function("allowed", decision_allowed);
    ctx.add_function("errored", decision_errored);
    ctx.add_function("error", decision_error);
    ctx.add_function("reason", decision_reason);
}

fn valid_dns_label(value: &str) -> bool {
    !value.is_empty() && value.len() <= 63 && value.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-') && !value.starts_with('-') && !value.ends_with('-')
}

fn valid_dns_subdomain(value: &str) -> bool {
    !value.is_empty() && value.len() <= 253 && value.split('.').all(valid_dns_label)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cel_ext;

    fn context(allowed: bool) -> AuthorizationContext {
        let rules = if allowed {
            vec![rbac::PolicyRule { verbs: vec!["get".to_string()], api_groups: vec!["apps".to_string()], resources: vec!["deployments".to_string()], ..Default::default() }]
        } else {
            Vec::new()
        };
        AuthorizationContext::from_rules(
            rules,
            "alice".to_string(),
            vec!["system:authenticated".to_string()],
            RequestResource { group: "apps".to_string(), resource: "deployments".to_string(), subresource: String::new(), namespace: "default".to_string(), name: "web".to_string(), path: String::new() },
        )
    }

    fn eval(expression: &str, context: AuthorizationContext) -> Value {
        let mut cel_context = Context::default();
        cel_context.add_variable("authorizer", value(context)).unwrap();
        cel_ext::register_kubernetes_extensions(&mut cel_context);
        cel::Program::compile(expression).unwrap().execute(&cel_context).unwrap()
    }

    #[test]
    fn request_resource_check_uses_the_request_rbac_rules() {
        assert_eq!(eval("authorizer.requestResource.check('get').allowed()", context(true)), Value::Bool(true));
        assert_eq!(eval("authorizer.requestResource.check('delete').allowed()", context(true)), Value::Bool(false));
    }

    #[test]
    fn fluent_group_resource_check_and_service_account_shape_work() {
        assert_eq!(eval("authorizer.group('apps').resource('deployments').namespace('default').name('web').check('get').allowed()", context(true)), Value::Bool(true));
        assert_eq!(eval("authorizer.serviceAccount('default', 'web').group('apps').resource('deployments').check('get').allowed()", context(true)), Value::Bool(false));
    }
}
