//! Storage-backed `MutatingAdmissionPolicy` enforcement.
//!
//! Policies and bindings are ordinary `admissionregistration.k8s.io/v1alpha1`
//! resources. The request path evaluates matching policies in deterministic
//! name order and applies each JSON Patch or apply configuration to the
//! candidate object before the remaining admission plugins run.
//!
//! The CEL runtime returns JSON-shaped values here. That keeps the mutation
//! engine independent from the storage codec while still accepting the same
//! object/array/map result shape that Kubernetes applies after evaluating a
//! policy expression.

use super::match_conditions::FailurePolicy;
use super::policy_decode::DecodedPolicy;
use super::policy_matching::{self, RequestVariable};
use super::validating_admission_policy::{self, PolicyDefinition, PolicyOutcome};
use crate::server::rest::{self, ListOutcome};
use crate::storage::client::StorageClient;
use serde_json::Value;
use std::collections::BTreeMap;

const GROUP: &str = "admissionregistration.k8s.io";
const VERSION: &str = "v1alpha1";

/// Apply all active mutating admission policies to one candidate object.
/// `object` is the candidate after earlier built-in admission mutations;
/// `old_object` is the pre-update object and is `None` for CREATE.
pub async fn mutate(
    storage: &mut StorageClient,
    operation: &str,
    group: &str,
    version: &str,
    resource: &str,
    subresource: &str,
    namespace: &str,
    name: &str,
    object: Value,
    old_object: Option<&Value>,
    dry_run: bool,
) -> Result<Value, String> {
    if is_exempt(group, resource) {
        return Ok(object);
    }

    let mut policies = list_items(storage, "mutatingadmissionpolicies").await?;
    let mut bindings = list_items(storage, "mutatingadmissionpolicybindings").await?;
    policies.sort_by(|left, right| object_name(left).cmp(&object_name(right)));
    bindings.sort_by(|left, right| object_name(left).cmp(&object_name(right)));
    if policies.is_empty() || bindings.is_empty() {
        return Ok(object);
    }

    let namespace_labels = if namespace.is_empty() {
        BTreeMap::new()
    } else {
        match rest::get(storage, None, "", "v1", "namespaces", None, namespace).await {
            Ok(rest::GetOutcome::Found(value)) => crate::cacher::selector::object_labels(&value),
            Ok(rest::GetOutcome::ObjectNotFound) | Ok(rest::GetOutcome::UnknownResource) => {
                BTreeMap::new()
            }
            Err(error) => {
                return Err(format!(
                    "looking up namespace labels for MutatingAdmissionPolicy: {error}"
                ))
            }
        }
    };
    let uid = uuid::Uuid::new_v4().to_string();
    let request = policy_matching::build_request_object(&RequestVariable {
        uid: &uid,
        group,
        version,
        resource,
        subresource,
        namespace,
        name,
        operation,
        dry_run,
    });

    let mut object = object;
    let old_labels = old_object
        .map(crate::cacher::selector::object_labels)
        .unwrap_or_default();
    for policy in &policies {
        let Some(policy_name) = object_name_ref(policy) else {
            continue;
        };
        let decoded = DecodedPolicy::decode(policy);
        let resource_rules = decoded.resource_rules();
        let exclude_resource_rules = decoded.exclude_resource_rules();
        let validations = [];
        let definition = PolicyDefinition {
            resource_rules: &resource_rules,
            exclude_resource_rules: &exclude_resource_rules,
            namespace_selector: decoded.namespace_selector,
            // The upstream object selector matches when either the new or
            // old object matches.  The pure evaluator accepts one label map,
            // so perform that two-object rule here and leave it out of the
            // borrowed definition below.
            object_selector: None,
            match_conditions: &decoded.match_conditions,
            validations: &validations,
            failure_policy: decoded.failure_policy,
        };

        for binding in bindings
            .iter()
            .filter(|binding| binding_policy_name(binding) == Some(policy_name))
        {
            if !binding_matches(
                binding,
                operation,
                group,
                version,
                resource,
                subresource,
                &namespace_labels,
                &crate::cacher::selector::object_labels(&object),
                &old_labels,
            ) {
                continue;
            }

            let parameter_values = match binding_parameters(storage, policy, binding).await {
                Ok(ParameterSelection::Values(values)) => values,
                Ok(ParameterSelection::Missing)
                    if decoded.failure_policy == FailurePolicy::Ignore =>
                {
                    continue
                }
                Ok(ParameterSelection::Missing) => {
                    return Err(format!(
                        "MutatingAdmissionPolicy {policy_name:?}: parameter was not found"
                    ))
                }
                Err(error) if decoded.failure_policy == FailurePolicy::Ignore => {
                    tracing::warn!(
                        policy = policy_name,
                        error,
                        "ignoring invalid MutatingAdmissionPolicy binding"
                    );
                    continue;
                }
                Err(error) => return Err(error),
            };

            for params in parameter_values {
                let labels = crate::cacher::selector::object_labels(&object);
                if !decoded.object_selector.map_or(true, |selector| {
                    policy_matching::matches_label_selector(Some(selector), &labels)
                        || policy_matching::matches_label_selector(Some(selector), &old_labels)
                }) {
                    continue;
                }
                let vars = policy_matching::build_eval_vars(
                    Some(&object),
                    old_object,
                    &request,
                    params.as_ref(),
                );
                match validating_admission_policy::evaluate(
                    &definition,
                    operation,
                    group,
                    version,
                    resource,
                    subresource,
                    &namespace_labels,
                    &labels,
                    &vars,
                ) {
                    PolicyOutcome::NotApplicable => continue,
                    PolicyOutcome::MatchConditionsError { errors } => {
                        return Err(format!(
                            "MutatingAdmissionPolicy {policy_name:?} matchConditions failed: {}",
                            errors.join("; ")
                        ))
                    }
                    PolicyOutcome::Decided(_) => {}
                }

                let mutations = policy
                    .get("spec")
                    .and_then(|spec| spec.get("mutations"))
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                let mut candidate = object.clone();
                for mutation in &mutations {
                    match apply_mutation(
                        storage, group, version, resource, &candidate, mutation, &vars,
                    )
                    .await
                    {
                        Ok(updated) => candidate = updated,
                        Err(error) if decoded.failure_policy == FailurePolicy::Ignore => {
                            tracing::warn!(
                                policy = policy_name,
                                error,
                                "ignoring failed MutatingAdmissionPolicy mutation"
                            );
                            candidate = object.clone();
                            break;
                        }
                        Err(error) => {
                            return Err(format!("MutatingAdmissionPolicy {policy_name:?}: {error}"))
                        }
                    }
                }
                object = candidate;
            }
        }
    }
    Ok(object)
}

async fn apply_mutation(
    storage: &mut StorageClient,
    group: &str,
    version: &str,
    resource: &str,
    object: &Value,
    mutation: &Value,
    vars: &[(&'static str, &Value)],
) -> Result<Value, String> {
    let patch_type = mutation
        .get("patchType")
        .and_then(Value::as_str)
        .ok_or_else(|| "mutation.patchType is required".to_string())?;
    match patch_type {
        "JSONPatch" => {
            let expression = mutation
                .pointer("/jsonPatch/expression")
                .and_then(Value::as_str)
                .ok_or_else(|| "JSONPatch mutation expression is required".to_string())?;
            let patch = crate::cel_ext::eval_json_with_vars_and_deadline(
                expression,
                vars,
                std::time::Duration::from_millis(100),
            )
            .map_err(|error| error.to_string())?;
            if !patch.is_array() {
                return Err("JSONPatch mutation expression must evaluate to an array".to_string());
            }
            let mut candidate = object.clone();
            crate::patch::json_patch::apply(&mut candidate, &patch)
                .map_err(|error| error.to_string())?;
            Ok(candidate)
        }
        "ApplyConfiguration" => {
            let expression = mutation
                .pointer("/applyConfiguration/expression")
                .and_then(Value::as_str)
                .ok_or_else(|| "ApplyConfiguration mutation expression is required".to_string())?;
            let configuration = crate::cel_ext::eval_json_with_vars_and_deadline(
                expression,
                vars,
                std::time::Duration::from_millis(100),
            )
            .map_err(|error| error.to_string())?;
            if !configuration.is_object() {
                return Err(
                    "ApplyConfiguration mutation expression must evaluate to an object".to_string(),
                );
            }
            rest::apply_admission_configuration(
                storage,
                group,
                version,
                resource,
                object,
                &configuration,
            )
            .await
            .map_err(|error| error.to_string())
        }
        other => Err(format!(
            "unsupported MutatingAdmissionPolicy patchType {other:?}"
        )),
    }
}

async fn list_items(storage: &mut StorageClient, resource: &str) -> Result<Vec<Value>, String> {
    match rest::list(storage, None, GROUP, VERSION, resource, None, "", "", 0, "").await {
        Ok(ListOutcome::Found(list)) => Ok(list
            .get("items")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()),
        Ok(ListOutcome::UnknownResource) | Ok(ListOutcome::InvalidContinueToken) => Ok(Vec::new()),
        Err(error) => Err(format!(
            "listing {GROUP}/{VERSION}/{resource} for admission: {error}"
        )),
    }
}

fn object_name(value: &Value) -> String {
    object_name_ref(value).unwrap_or_default().to_string()
}

fn object_name_ref(value: &Value) -> Option<&str> {
    value.pointer("/metadata/name").and_then(Value::as_str)
}

fn binding_policy_name(binding: &Value) -> Option<&str> {
    binding.pointer("/spec/policyName").and_then(Value::as_str)
}

fn is_exempt(group: &str, resource: &str) -> bool {
    (group == GROUP
        && matches!(
            resource,
            "validatingadmissionpolicies"
                | "validatingadmissionpolicybindings"
                | "mutatingadmissionpolicies"
                | "mutatingadmissionpolicybindings"
        ))
        || (group == "authentication.k8s.io" && resource == "tokenreviews")
        || (group == "authorization.k8s.io"
            && matches!(
                resource,
                "selfsubjectreviews"
                    | "localsubjectaccessreviews"
                    | "selfsubjectaccessreviews"
                    | "selfsubjectrulesreviews"
                    | "subjectaccessreviews"
            ))
}

fn binding_matches(
    binding: &Value,
    operation: &str,
    group: &str,
    version: &str,
    resource: &str,
    subresource: &str,
    namespace_labels: &BTreeMap<String, String>,
    object_labels: &BTreeMap<String, String>,
    old_object_labels: &BTreeMap<String, String>,
) -> bool {
    let Some(match_resources) = binding.pointer("/spec/matchResources") else {
        return true;
    };
    let include = match_resources
        .get("resourceRules")
        .and_then(Value::as_array)
        .map_or(true, |rules| {
            rules.is_empty()
                || rules.iter().any(|rule| {
                    raw_rule_matches(rule, operation, group, version, resource, subresource)
                })
        });
    let excluded = match_resources
        .get("excludeResourceRules")
        .and_then(Value::as_array)
        .is_some_and(|rules| {
            rules.iter().any(|rule| {
                raw_rule_matches(rule, operation, group, version, resource, subresource)
            })
        });
    include
        && !excluded
        && selector_matches(match_resources.get("namespaceSelector"), namespace_labels)
        && (selector_matches(match_resources.get("objectSelector"), object_labels)
            || selector_matches(match_resources.get("objectSelector"), old_object_labels))
}

fn raw_rule_matches(
    rule: &Value,
    operation: &str,
    group: &str,
    version: &str,
    resource: &str,
    subresource: &str,
) -> bool {
    let contains = |field: &str, actual: &str| {
        rule.get(field)
            .and_then(Value::as_array)
            .is_some_and(|values| {
                values.iter().any(|value| {
                    value
                        .as_str()
                        .is_some_and(|candidate| candidate == "*" || candidate == actual)
                })
            })
    };
    let resource_name = if subresource.is_empty() {
        resource.to_string()
    } else {
        format!("{resource}/{subresource}")
    };
    contains("operations", operation)
        && contains("apiGroups", group)
        && contains("apiVersions", version)
        && contains("resources", &resource_name)
}

fn selector_matches(selector: Option<&Value>, labels: &BTreeMap<String, String>) -> bool {
    selector.map_or(true, |value| {
        policy_matching::matches_label_selector(Some(value), labels)
    })
}

enum ParameterSelection {
    Values(Vec<Option<Value>>),
    Missing,
}

async fn binding_parameters(
    storage: &mut StorageClient,
    policy: &Value,
    binding: &Value,
) -> Result<ParameterSelection, String> {
    let Some(param_kind) = policy.pointer("/spec/paramKind") else {
        return Ok(ParameterSelection::Values(vec![None]));
    };
    let Some(param_ref) = binding.pointer("/spec/paramRef") else {
        return Ok(ParameterSelection::Values(vec![None]));
    };
    let api_group = param_kind
        .get("apiGroup")
        .and_then(Value::as_str)
        .unwrap_or("");
    let kind = param_kind
        .get("kind")
        .and_then(Value::as_str)
        .filter(|kind| !kind.is_empty())
        .ok_or_else(|| "MutatingAdmissionPolicy.spec.paramKind.kind is missing".to_string())?;
    let allow_missing = param_ref
        .get("parameterNotFoundAction")
        .and_then(Value::as_str)
        == Some("Allow");
    let Some((resolved_group, version, resource, namespaced)) =
        rest::resolve_resource_for_kind(storage, api_group, kind)
            .await
            .map_err(|error| {
                format!(
                    "resolving MutatingAdmissionPolicy parameter kind {api_group}/{kind}: {error}"
                )
            })?
    else {
        return if allow_missing {
            Ok(ParameterSelection::Values(Vec::new()))
        } else {
            Ok(ParameterSelection::Missing)
        };
    };
    let requested_namespace = param_ref
        .get("namespace")
        .and_then(Value::as_str)
        .filter(|namespace| !namespace.is_empty());
    if !namespaced && requested_namespace.is_some() {
        return Err(format!("cluster-scoped MutatingAdmissionPolicy parameter kind {resolved_group}/{kind} cannot use paramRef.namespace"));
    }
    let namespace = if namespaced {
        requested_namespace
    } else {
        None
    };
    let selected = if let Some(name) = param_ref
        .get("name")
        .and_then(Value::as_str)
        .filter(|name| !name.is_empty())
    {
        if namespaced && namespace.is_none() {
            list_resource_items(storage, &resolved_group, &version, &resource, None)
                .await?
                .into_iter()
                .filter(|object| {
                    object.pointer("/metadata/name").and_then(Value::as_str) == Some(name)
                })
                .collect()
        } else {
            match rest::get(storage, None, &resolved_group, &version, &resource, namespace, name).await {
                Ok(rest::GetOutcome::Found(object)) => vec![object],
                Ok(rest::GetOutcome::ObjectNotFound) | Ok(rest::GetOutcome::UnknownResource) => Vec::new(),
                Err(error) => return Err(format!("reading MutatingAdmissionPolicy parameter {resolved_group}/{resource}/{name}: {error}")),
            }
        }
    } else if let Some(selector) = param_ref.get("selector") {
        list_resource_items(storage, &resolved_group, &version, &resource, namespace)
            .await?
            .into_iter()
            .filter(|object| {
                policy_matching::matches_label_selector(
                    Some(selector),
                    &crate::cacher::selector::object_labels(object),
                )
            })
            .collect()
    } else {
        Vec::new()
    };
    if selected.is_empty() && !allow_missing {
        return Ok(ParameterSelection::Missing);
    }
    Ok(ParameterSelection::Values(
        selected.into_iter().map(Some).collect(),
    ))
}

async fn list_resource_items(
    storage: &mut StorageClient,
    group: &str,
    version: &str,
    resource: &str,
    namespace: Option<&str>,
) -> Result<Vec<Value>, String> {
    match rest::list(
        storage, None, group, version, resource, namespace, "", "", 0, "",
    )
    .await
    {
        Ok(ListOutcome::Found(list)) => Ok(list
            .get("items")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()),
        Ok(ListOutcome::UnknownResource) | Ok(ListOutcome::InvalidContinueToken) => Ok(Vec::new()),
        Err(error) => Err(format!(
            "listing {group}/{version}/{resource} for admission: {error}"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn binding_without_match_resources_matches() {
        assert!(binding_matches(
            &json!({"spec": {}}),
            "CREATE",
            "",
            "v1",
            "configmaps",
            "",
            &BTreeMap::new(),
            &BTreeMap::new(),
            &BTreeMap::new()
        ));
    }

    #[test]
    fn binding_resource_rules_match_subresources() {
        let binding = json!({"spec": {"matchResources": {"resourceRules": [{"operations": ["UPDATE"], "apiGroups": ["apps"], "apiVersions": ["v1"], "resources": ["deployments/status"]}]}}});
        assert!(binding_matches(
            &binding,
            "UPDATE",
            "apps",
            "v1",
            "deployments",
            "status",
            &BTreeMap::new(),
            &BTreeMap::new(),
            &BTreeMap::new()
        ));
        assert!(!binding_matches(
            &binding,
            "UPDATE",
            "apps",
            "v1",
            "deployments",
            "",
            &BTreeMap::new(),
            &BTreeMap::new()
        ));
    }

    #[test]
    fn object_selector_matches_either_side_of_an_update() {
        let binding = json!({
            "spec": {
                "matchResources": {
                    "objectSelector": {"matchLabels": {"managed": "yes"}}
                }
            }
        });
        let new_labels = BTreeMap::new();
        let old_labels = [("managed".to_string(), "yes".to_string())]
            .into_iter()
            .collect();
        assert!(binding_matches(
            &binding,
            "UPDATE",
            "",
            "v1",
            "configmaps",
            "",
            &BTreeMap::new(),
            &new_labels,
            &old_labels,
        ));
    }

    #[test]
    fn admission_configuration_resources_are_exempt() {
        assert!(is_exempt(GROUP, "mutatingadmissionpolicies"));
        assert!(is_exempt("authorization.k8s.io", "subjectaccessreviews"));
        assert!(!is_exempt("", "configmaps"));
    }
}
