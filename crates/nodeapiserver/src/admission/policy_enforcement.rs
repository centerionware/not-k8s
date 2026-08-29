//! Storage-backed `ValidatingAdmissionPolicy` enforcement.
//!
//! The policy modules deliberately expose pure, upstream-shaped primitives.
//! This module is the small adapter that loads policies and bindings from
//! the same storage path as ordinary REST requests, builds the request CEL
//! variables, and turns bound validation actions into admission results.
//! Parameter references are resolved through the ordinary REST storage path;
//! warnings and audit annotations are carried back to the request wrapper.

use super::policy_decode::DecodedPolicy;
use super::policy_matching::{self, RequestVariable};
use super::validating_admission_policy;
use crate::server::rest::{self, ListOutcome};
use crate::storage::client::StorageClient;
use serde_json::Value;
use std::collections::BTreeMap;

const GROUP: &str = "admissionregistration.k8s.io";
const VERSION: &str = "v1";
pub const VALIDATION_FAILURE_AUDIT_ANNOTATION: &str = "validation.policy.admission.k8s.io/validation_failure";

/// The complete result of evaluating all matching VAP bindings for one
/// request. A binding may report a warning or audit annotation even when
/// another binding denies the request, so these results are accumulated
/// instead of returning on the first failure.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ValidationOutcome {
    pub denial: Option<String>,
    pub warnings: Vec<String>,
    pub audit_failures: Vec<Value>,
}

/// Evaluate every matching policy binding for one mutating request.
pub async fn validate(
    storage: &mut StorageClient,
    operation: &str,
    group: &str,
    version: &str,
    resource: &str,
    subresource: &str,
    namespace: &str,
    name: &str,
    object: Option<&Value>,
    old_object: Option<&Value>,
    dry_run: bool,
) -> Result<ValidationOutcome, String> {
    let mut result = ValidationOutcome::default();
    let policies = list_items(storage, "validatingadmissionpolicies", None).await?;
    if policies.is_empty() {
        return Ok(result);
    }
    let bindings = list_items(storage, "validatingadmissionpolicybindings", None).await?;
    if bindings.is_empty() {
        return Ok(result);
    }

    let namespace_labels = if namespace.is_empty() {
        BTreeMap::new()
    } else {
        match rest::get(storage, None, "", "v1", "namespaces", None, namespace).await {
            Ok(rest::GetOutcome::Found(object)) => crate::cacher::selector::object_labels(&object),
            Ok(rest::GetOutcome::ObjectNotFound) | Ok(rest::GetOutcome::UnknownResource) => BTreeMap::new(),
            Err(error) => return Err(format!("looking up namespace labels for ValidatingAdmissionPolicy: {error}")),
        }
    };
    // On DELETE, Kubernetes evaluates object selectors against the existing
    // object (`oldObject`); the request object itself is intentionally null.
    let object_labels = object.or(old_object).map(crate::cacher::selector::object_labels).unwrap_or_default();
    let request = policy_matching::build_request_object(&RequestVariable {
        uid: "",
        group,
        version,
        resource,
        subresource,
        namespace,
        name,
        operation,
        dry_run,
    });

    for policy in &policies {
        let Some(policy_name) = policy.get("metadata").and_then(|m| m.get("name")).and_then(Value::as_str) else {
            continue;
        };
        let decoded = DecodedPolicy::decode(policy);
        let resource_rules = decoded.resource_rules();
        let exclude_resource_rules = decoded.exclude_resource_rules();
        for binding in bindings.iter().filter(|binding| binding_policy_name(binding) == Some(policy_name)) {
            let actions = binding_actions(binding);
            if !validating_admission_policy::validation_actions_report(&actions) {
                continue;
            }
            if !binding_matches(binding, operation, group, version, resource, subresource, &namespace_labels, &object_labels) {
                continue;
            }
            let parameter_values = match binding_parameters(storage, policy, binding).await? {
                ParameterSelection::Values(values) => values,
                ParameterSelection::Missing if matches!(decoded.failure_policy, super::match_conditions::FailurePolicy::Ignore) => continue,
                ParameterSelection::Missing => {
                    record_failure(
                        &mut result,
                        policy_name,
                        binding,
                        &actions,
                        "parameter was not found".to_string(),
                        None,
                    );
                    continue;
                }
            };
            for params in parameter_values {
                let vars = policy_matching::build_eval_vars(object, old_object, &request, params.as_ref());
                let definition = validating_admission_policy::PolicyDefinition {
                    resource_rules: &resource_rules,
                    exclude_resource_rules: &exclude_resource_rules,
                    namespace_selector: decoded.namespace_selector,
                    object_selector: decoded.object_selector,
                    match_conditions: &decoded.match_conditions,
                    validations: &decoded.validations,
                    failure_policy: decoded.failure_policy,
                };
                let outcome = validating_admission_policy::evaluate(&definition, operation, group, version, resource, subresource, &namespace_labels, &object_labels, &vars);
                match outcome {
                    validating_admission_policy::PolicyOutcome::MatchConditionsError { errors } => {
                        record_failure(&mut result, policy_name, binding, &actions, errors.join("; "), None);
                    }
                    validating_admission_policy::PolicyOutcome::Decided(decisions) => {
                        for (expression_index, decision) in decisions.into_iter().enumerate() {
                            if decision.action != super::policy_validations::Action::Deny {
                                continue;
                            }
                            let message = decision.message.unwrap_or_else(|| format!("failed validation at index {expression_index}"));
                            record_failure(&mut result, policy_name, binding, &actions, message, Some(expression_index));
                        }
                    }
                    validating_admission_policy::PolicyOutcome::NotApplicable => {}
                }
            }
        }
    }
    Ok(result)
}

fn record_failure(result: &mut ValidationOutcome, policy_name: &str, binding: &Value, actions: &[&str], message: String, expression_index: Option<usize>) {
    let detail = format!("ValidatingAdmissionPolicy {policy_name:?}: {message}");
    if validating_admission_policy::validation_actions_deny(actions) && result.denial.is_none() {
        result.denial = Some(detail.clone());
    }
    if validating_admission_policy::validation_actions_warn(actions) {
        result.warnings.push(detail);
    }
    if validating_admission_policy::validation_actions_audit(actions) {
        let binding_name = binding.get("metadata").and_then(|metadata| metadata.get("name")).and_then(Value::as_str).unwrap_or("");
        result.audit_failures.push(serde_json::json!({
            "message": message,
            "policy": policy_name,
            "binding": binding_name,
            "expressionIndex": expression_index,
            "validationActions": actions,
        }));
    }
}

async fn list_items(storage: &mut StorageClient, resource: &str, namespace: Option<&str>) -> Result<Vec<Value>, String> {
    list_resource_items(storage, GROUP, VERSION, resource, namespace).await
}

async fn list_resource_items(storage: &mut StorageClient, group: &str, version: &str, resource: &str, namespace: Option<&str>) -> Result<Vec<Value>, String> {
    match rest::list(storage, None, group, version, resource, namespace, "", "", 0, "").await {
        Ok(ListOutcome::Found(list)) => Ok(list.get("items").and_then(Value::as_array).cloned().unwrap_or_default()),
        Ok(ListOutcome::UnknownResource) | Ok(ListOutcome::InvalidContinueToken) => Ok(Vec::new()),
        Err(error) => Err(format!("listing {group}/{version}/{resource} for admission: {error}")),
    }
}

/// Resolve one binding's optional parameter reference. A missing parameter
/// is an admission error by default, matching `parameterNotFoundAction:
/// Deny`; `Allow` skips that binding. A selector may select more than one
/// parameter object, in which case the policy must pass for every selected
/// parameter, matching upstream's parameterized-policy semantics.
enum ParameterSelection {
    Values(Vec<Option<Value>>),
    Missing,
}

async fn binding_parameters(storage: &mut StorageClient, policy: &Value, binding: &Value) -> Result<ParameterSelection, String> {
    let Some(param_ref) = binding.get("spec").and_then(|spec| spec.get("paramRef")) else {
        return Ok(ParameterSelection::Values(vec![None]));
    };
    let Some(param_kind) = policy.get("spec").and_then(|spec| spec.get("paramKind")) else {
        return Err("ValidatingAdmissionPolicyBinding has a paramRef but its policy has no paramKind".to_string());
    };
    let api_group = param_kind.get("apiGroup").and_then(Value::as_str).unwrap_or("");
    let kind = param_kind.get("kind").and_then(Value::as_str).filter(|kind| !kind.is_empty()).ok_or_else(|| "ValidatingAdmissionPolicy.paramKind.kind is missing".to_string())?;
    let allow_missing = param_ref.get("parameterNotFoundAction").and_then(Value::as_str) == Some("Allow");
    let Some((resolved_group, version, resource, namespaced)) = rest::resolve_resource_for_kind(storage, api_group, kind).await.map_err(|error| format!("resolving ValidatingAdmissionPolicy parameter kind {api_group}/{kind}: {error}"))? else {
        return if allow_missing {
            Ok(ParameterSelection::Values(Vec::new()))
        } else {
            Err(format!("ValidatingAdmissionPolicy parameter kind {api_group}/{kind} is not served"))
        };
    };
    let requested_namespace = param_ref.get("namespace").and_then(Value::as_str).filter(|namespace| !namespace.is_empty());
    if !namespaced && requested_namespace.is_some() {
        return Err(format!("cluster-scoped ValidatingAdmissionPolicy parameter kind {resolved_group}/{kind} cannot use paramRef.namespace"));
    }
    let namespace = if namespaced { requested_namespace } else { None };
    let selected = if let Some(name) = param_ref.get("name").and_then(Value::as_str).filter(|name| !name.is_empty()) {
        if namespaced && namespace.is_none() {
            list_resource_items(storage, &resolved_group, &version, &resource, None).await?.into_iter().filter(|object| object.pointer("/metadata/name").and_then(Value::as_str) == Some(name)).collect()
        } else {
            match rest::get(storage, None, &resolved_group, &version, &resource, namespace, name).await {
                Ok(rest::GetOutcome::Found(object)) => vec![object],
                Ok(rest::GetOutcome::ObjectNotFound) | Ok(rest::GetOutcome::UnknownResource) => Vec::new(),
                Err(error) => return Err(format!("reading ValidatingAdmissionPolicy parameter {resolved_group}/{resource}/{name}: {error}")),
            }
        }
    } else if let Some(selector) = param_ref.get("selector") {
        list_resource_items(storage, &resolved_group, &version, &resource, namespace)
            .await?
            .into_iter()
            .filter(|object| policy_matching::matches_label_selector(Some(selector), &crate::cacher::selector::object_labels(object)))
            .collect()
    } else {
        Vec::new()
    };
    if selected.is_empty() && !allow_missing {
        return Ok(ParameterSelection::Missing);
    }
    Ok(ParameterSelection::Values(selected.into_iter().map(Some).collect()))
}

fn binding_policy_name(binding: &Value) -> Option<&str> {
    binding.get("spec").and_then(|s| s.get("policyName")).and_then(Value::as_str)
}

fn binding_actions(binding: &Value) -> Vec<&str> {
    binding
        .get("spec")
        .and_then(|s| s.get("validationActions"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect()
}

/// Binding matchResources has the same rule and selector shape as the
/// policy's matchConstraints. Keep this local adapter value-based so it does
/// not duplicate the policy decoder's self-referential borrow shape.
fn binding_matches(
    binding: &Value,
    operation: &str,
    group: &str,
    version: &str,
    resource: &str,
    subresource: &str,
    namespace_labels: &BTreeMap<String, String>,
    object_labels: &BTreeMap<String, String>,
) -> bool {
    let Some(match_resources) = binding.get("spec").and_then(|s| s.get("matchResources")) else {
        return true;
    };
    let include = match_resources.get("resourceRules").and_then(Value::as_array).map_or(true, |rules| {
        rules.is_empty() || rules.iter().any(|rule| raw_rule_matches(rule, operation, group, version, resource, subresource))
    });
    let excluded = match_resources.get("excludeResourceRules").and_then(Value::as_array).is_some_and(|rules| rules.iter().any(|rule| raw_rule_matches(rule, operation, group, version, resource, subresource)));
    include
        && !excluded
        && selector_matches(match_resources.get("namespaceSelector"), namespace_labels)
        && selector_matches(match_resources.get("objectSelector"), object_labels)
}

fn raw_rule_matches(rule: &Value, operation: &str, group: &str, version: &str, resource: &str, subresource: &str) -> bool {
    let contains = |field: &str, value: &str| rule.get(field).and_then(Value::as_array).is_some_and(|values| values.iter().any(|v| v.as_str().is_some_and(|candidate| candidate == "*" || candidate == value)));
    let resource_name = if subresource.is_empty() { resource.to_string() } else { format!("{resource}/{subresource}") };
    contains("operations", operation) && contains("apiGroups", group) && contains("apiVersions", version) && contains("resources", &resource_name)
}

fn selector_matches(selector: Option<&Value>, labels: &BTreeMap<String, String>) -> bool {
    selector.map_or(true, |value| policy_matching::matches_label_selector(Some(value), labels))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn binding_resource_rule_matches_the_real_request_shape() {
        let binding = json!({"spec": {"matchResources": {"resourceRules": [{"operations": ["CREATE"], "apiGroups": ["apps"], "apiVersions": ["v1"], "resources": ["deployments"]}]}}});
        assert!(binding_matches(&binding, "CREATE", "apps", "v1", "deployments", "", &BTreeMap::new(), &BTreeMap::new()));
        assert!(!binding_matches(&binding, "UPDATE", "apps", "v1", "deployments", "", &BTreeMap::new(), &BTreeMap::new()));
    }

    #[test]
    fn a_binding_without_match_resources_matches_everything() {
        assert!(binding_matches(&json!({"spec": {}}), "CREATE", "", "v1", "pods", "", &BTreeMap::new(), &BTreeMap::new()));
    }

    #[test]
    fn a_warn_only_failure_is_reported_without_becoming_a_denial() {
        let mut result = ValidationOutcome::default();
        let binding = json!({"metadata": {"name": "pod-policy-binding"}});
        record_failure(&mut result, "pod-policy", &binding, &["Warn"], "pods are discouraged".to_string(), Some(0));
        assert_eq!(result.denial, None);
        assert_eq!(result.warnings, vec!["ValidatingAdmissionPolicy \"pod-policy\": pods are discouraged"]);
        assert!(result.audit_failures.is_empty());
    }

    #[test]
    fn an_audit_failure_has_the_upstream_annotation_fields() {
        let mut result = ValidationOutcome::default();
        let binding = json!({"metadata": {"name": "pod-policy-binding"}});
        record_failure(&mut result, "pod-policy", &binding, &["Audit"], "pods are discouraged".to_string(), Some(2));
        assert_eq!(result.denial, None);
        assert_eq!(result.warnings, Vec::<String>::new());
        assert_eq!(result.audit_failures[0]["message"], "pods are discouraged");
        assert_eq!(result.audit_failures[0]["policy"], "pod-policy");
        assert_eq!(result.audit_failures[0]["binding"], "pod-policy-binding");
        assert_eq!(result.audit_failures[0]["expressionIndex"], 2);
        assert_eq!(result.audit_failures[0]["validationActions"], json!(["Audit"]));
    }
}
