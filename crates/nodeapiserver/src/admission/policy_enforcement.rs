//! Storage-backed `ValidatingAdmissionPolicy` enforcement.
//!
//! The policy modules deliberately expose pure, upstream-shaped primitives.
//! This module is the small adapter that loads policies and bindings from
//! the same storage path as ordinary REST requests, builds the request CEL
//! variables, and turns a bound `Deny` action into one admission message.
//! Parameter references and warning/audit actions remain explicit follow-up
//! work; a binding with a parameter reference is failed closed rather than
//! evaluated with an incorrect `null` parameter.

use super::policy_decode::DecodedPolicy;
use super::policy_matching::{self, RequestVariable};
use super::validating_admission_policy;
use crate::server::rest::{self, ListOutcome};
use crate::storage::client::StorageClient;
use serde_json::Value;
use std::collections::BTreeMap;

const GROUP: &str = "admissionregistration.k8s.io";
const VERSION: &str = "v1";

/// Evaluate every bound, deny-capable policy for one mutating request.
/// `Some` is the first rejection message; `None` means no binding denied.
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
) -> Result<Option<String>, String> {
    let policies = list_items(storage, "validatingadmissionpolicies", None).await?;
    if policies.is_empty() {
        return Ok(None);
    }
    let bindings = list_items(storage, "validatingadmissionpolicybindings", None).await?;
    if bindings.is_empty() {
        return Ok(None);
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
    let object_labels = object.map(crate::cacher::selector::object_labels).unwrap_or_default();
    let request = policy_matching::build_request_object(&RequestVariable {
        uid: "",
        group,
        version,
        resource,
        subresource,
        namespace,
        name,
        operation,
        dry_run: false,
    });
    let vars = policy_matching::build_eval_vars(object, old_object, &request, None);

    for policy in &policies {
        let Some(policy_name) = policy.get("metadata").and_then(|m| m.get("name")).and_then(Value::as_str) else {
            continue;
        };
        let decoded = DecodedPolicy::decode(policy);
        let resource_rules = decoded.resource_rules();
        let exclude_resource_rules = decoded.exclude_resource_rules();
        for binding in bindings.iter().filter(|binding| binding_policy_name(binding) == Some(policy_name)) {
            let actions = binding_actions(binding);
            if !validating_admission_policy::validation_actions_deny(&actions) {
                continue;
            }
            if binding.get("spec").and_then(|s| s.get("paramRef")).is_some() {
                return Err(format!("ValidatingAdmissionPolicyBinding for {policy_name:?} uses unsupported paramRef"));
            }
            if !binding_matches(binding, operation, group, version, resource, subresource, &namespace_labels, &object_labels) {
                continue;
            }
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
            if outcome.is_denial() {
                let message = outcome.denial_message().unwrap_or_else(|| format!("ValidatingAdmissionPolicy {policy_name:?} denied the request"));
                return Ok(Some(format!("ValidatingAdmissionPolicy {policy_name:?}: {message}")));
            }
        }
    }
    Ok(None)
}

async fn list_items(storage: &mut StorageClient, resource: &str, namespace: Option<&str>) -> Result<Vec<Value>, String> {
    match rest::list(storage, None, GROUP, VERSION, resource, namespace, "", "", 0, "").await {
        Ok(ListOutcome::Found(list)) => Ok(list.get("items").and_then(Value::as_array).cloned().unwrap_or_default()),
        Ok(ListOutcome::UnknownResource) | Ok(ListOutcome::InvalidContinueToken) => Ok(Vec::new()),
        Err(error) => Err(format!("listing {GROUP}/{VERSION}/{resource} for admission: {error}")),
    }
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
}
