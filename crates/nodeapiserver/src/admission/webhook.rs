//! Mutating and validating admission webhooks.
//!
//! This is the request-side half of the real upstream webhook admission
//! plugin: configurations are read from the apiserver's own storage, rules
//! and selectors are evaluated against the request, and the selected service
//! receives an `admission.k8s.io/v1` (or v1beta1) `AdmissionReview`. Mutating
//! responses may return the standard base64-encoded JSON Patch; validating
//! responses may allow or deny the request.
//!
//! The service target is resolved to its ClusterIP before dialing. The URL's
//! host remains the service DNS name for TLS SNI and certificate validation,
//! while reqwest's resolver override connects to the stored ClusterIP. This
//! avoids making apiserver admission depend on the host's `/etc/resolv.conf`
//! containing the cluster DNS service.

use crate::admission::attributes::Operation;
use crate::authn::x509::Identity;
use crate::cacher::selector;
use crate::server::rest::{self, ListOutcome};
use crate::storage::client::StorageClient;
use base64::Engine;
use reqwest::{Certificate, Client};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

const ADMISSION_GROUP: &str = "admissionregistration.k8s.io";
const ADMISSION_VERSION: &str = "v1";
const DEFAULT_WEBHOOK_PORT: u16 = 443;
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug)]
pub enum Outcome {
    Allowed(Value),
    Denied(String),
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("reading admission webhook configuration failed: {0}")]
    Storage(#[from] rest::Error),
    #[error("invalid admission webhook {webhook}: {detail}")]
    Invalid { webhook: String, detail: String },
    #[error("admission webhook {webhook} failed: {detail}")]
    Invocation { webhook: String, detail: String },
}

#[derive(Debug)]
struct Endpoint {
    url: String,
    resolve: Option<(String, SocketAddr)>,
}

/// Run all matching mutating webhooks followed by all matching validating
/// webhooks. Configuration and webhook order is sorted by name, matching the
/// deterministic order used by upstream's webhook dispatcher. A failure is
/// ignored only when that webhook explicitly sets `failurePolicy: Ignore`;
/// an explicit admission denial is never ignored.
pub async fn admit(
    storage: &mut StorageClient,
    operation: Operation,
    group: &str,
    version: &str,
    resource: &str,
    subresource: &str,
    namespace: &str,
    name: &str,
    object: Value,
    old_object: Option<Value>,
    identity: Option<&Identity>,
    dry_run: bool,
) -> Result<Outcome, Error> {
    let mutating = list_configurations(storage, "mutatingwebhookconfigurations").await?;
    let validating = list_configurations(storage, "validatingwebhookconfigurations").await?;
    let namespace_object = if namespace.is_empty() {
        None
    } else {
        match rest::get(storage, None, "", "v1", "namespaces", None, namespace).await? {
            rest::GetOutcome::Found(value) => Some(value),
            rest::GetOutcome::ObjectNotFound | rest::GetOutcome::UnknownResource => None,
        }
    };
    let uid = uuid::Uuid::new_v4().to_string();
    let mut object = object;

    for configuration in mutating {
        let mut webhooks = sorted_webhooks(&configuration);
        for webhook in webhooks.drain(..) {
            if !matches_webhook(
                &webhook,
                operation,
                group,
                version,
                resource,
                subresource,
                namespace,
                &object,
                namespace_object.as_ref(),
                old_object.as_ref(),
                &uid,
                name,
                identity,
                dry_run,
            )? {
                continue;
            }
            match invoke(
                storage,
                &webhook,
                true,
                &uid,
                operation,
                group,
                version,
                resource,
                subresource,
                namespace,
                name,
                &object,
                old_object.as_ref(),
                identity,
                dry_run,
            )
            .await
            {
                Ok(Invocation::Allowed(updated)) => object = updated,
                Ok(Invocation::Denied(message)) => return Ok(Outcome::Denied(message)),
                Err(error) if failure_policy_ignore(&webhook) => {
                    tracing::warn!(error = ?error, "ignoring failed mutating admission webhook")
                }
                Err(error) => return Err(error),
            }
        }
    }

    for configuration in validating {
        let mut webhooks = sorted_webhooks(&configuration);
        for webhook in webhooks.drain(..) {
            if !matches_webhook(
                &webhook,
                operation,
                group,
                version,
                resource,
                subresource,
                namespace,
                &object,
                namespace_object.as_ref(),
                old_object.as_ref(),
                &uid,
                name,
                identity,
                dry_run,
            )? {
                continue;
            }
            match invoke(
                storage,
                &webhook,
                false,
                &uid,
                operation,
                group,
                version,
                resource,
                subresource,
                namespace,
                name,
                &object,
                old_object.as_ref(),
                identity,
                dry_run,
            )
            .await
            {
                Ok(Invocation::Allowed(updated)) => object = updated,
                Ok(Invocation::Denied(message)) => return Ok(Outcome::Denied(message)),
                Err(error) if failure_policy_ignore(&webhook) => {
                    tracing::warn!(error = ?error, "ignoring failed validating admission webhook")
                }
                Err(error) => return Err(error),
            }
        }
    }

    Ok(Outcome::Allowed(object))
}

async fn list_configurations(
    storage: &mut StorageClient,
    resource: &str,
) -> Result<Vec<Value>, Error> {
    let result = rest::list(
        storage,
        None,
        ADMISSION_GROUP,
        ADMISSION_VERSION,
        resource,
        None,
        "",
        "",
        0,
        "",
    )
    .await?;
    let ListOutcome::Found(list) = result else {
        return Ok(Vec::new());
    };
    let mut configurations = list
        .get("items")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    configurations.sort_by(|left, right| object_name(left).cmp(&object_name(right)));
    Ok(configurations)
}

fn sorted_webhooks(configuration: &Value) -> Vec<Value> {
    let mut webhooks = configuration
        .get("webhooks")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    webhooks.sort_by(|left, right| object_name(left).cmp(&object_name(right)));
    webhooks
}

fn object_name(value: &Value) -> String {
    value
        .pointer("/metadata/name")
        .or_else(|| value.get("name"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

fn failure_policy_ignore(webhook: &Value) -> bool {
    webhook.get("failurePolicy").and_then(Value::as_str) == Some("Ignore")
}

fn matches_webhook(
    webhook: &Value,
    operation: Operation,
    group: &str,
    version: &str,
    resource: &str,
    subresource: &str,
    namespace: &str,
    object: &Value,
    namespace_object: Option<&Value>,
    old_object: Option<&Value>,
    uid: &str,
    name: &str,
    identity: Option<&Identity>,
    dry_run: bool,
) -> Result<bool, Error> {
    let Some(rules) = webhook.get("rules").and_then(Value::as_array) else {
        return Ok(false);
    };
    if !rules.iter().any(|rule| {
        rule_matches(
            rule,
            operation,
            group,
            version,
            resource,
            subresource,
            namespace,
        )
    }) {
        return Ok(false);
    }
    if !selector_matches(
        webhook.get("objectSelector"),
        &selector::object_labels(object),
    )? {
        return Ok(false);
    }
    if let Some(selector_value) = webhook.get("namespaceSelector") {
        let labels = namespace_object
            .map(selector::object_labels)
            .unwrap_or_default();
        if namespace.is_empty() && !selector_is_empty(selector_value) {
            return Ok(false);
        }
        if !selector_matches(Some(selector_value), &labels)? {
            return Ok(false);
        }
    }

    let Some(conditions) = webhook.get("matchConditions").and_then(Value::as_array) else {
        return Ok(true);
    };
    if conditions.is_empty() {
        return Ok(true);
    }

    let webhook_name = object_name(webhook);
    let mut parsed = Vec::with_capacity(conditions.len());
    for condition in conditions {
        let condition_name = condition
            .get("name")
            .and_then(Value::as_str)
            .ok_or_else(|| Error::Invalid {
                webhook: webhook_name.clone(),
                detail: "matchConditions entry has no name".to_string(),
            })?;
        let expression = condition
            .get("expression")
            .and_then(Value::as_str)
            .ok_or_else(|| Error::Invalid {
                webhook: webhook_name.clone(),
                detail: format!("matchCondition {condition_name:?} has no expression"),
            })?;
        parsed.push(crate::admission::match_conditions::MatchCondition {
            name: condition_name,
            expression,
        });
    }

    let mut request = crate::admission::policy_matching::build_request_object(
        &crate::admission::policy_matching::RequestVariable {
            uid,
            group,
            version,
            resource,
            subresource,
            namespace,
            name,
            operation: operation_name(operation),
            dry_run,
        },
    );
    let kind = object.get("kind").cloned().unwrap_or(Value::Null);
    request["kind"]["kind"] = kind.clone();
    request["requestKind"]["kind"] = kind;
    request["userInfo"] = user_info(identity);
    let old_object = old_object.cloned().unwrap_or(Value::Null);
    let vars = [("object", object), ("oldObject", &old_object), ("request", &request)];
    let failure_policy = if webhook.get("failurePolicy").and_then(Value::as_str) == Some("Ignore") {
        crate::admission::match_conditions::FailurePolicy::Ignore
    } else {
        crate::admission::match_conditions::FailurePolicy::Fail
    };
    match crate::admission::match_conditions::match_conditions(&parsed, &vars, failure_policy) {
        crate::admission::match_conditions::MatchResult::Matches => Ok(true),
        crate::admission::match_conditions::MatchResult::DoesNotMatch { .. }
        | crate::admission::match_conditions::MatchResult::Ignored { .. } => Ok(false),
        crate::admission::match_conditions::MatchResult::Error { errors } => Err(Error::Invalid {
            webhook: webhook_name,
            detail: format!("matchConditions evaluation failed: {}", errors.join("; ")),
        }),
    }
}

fn rule_matches(
    rule: &Value,
    operation: Operation,
    group: &str,
    version: &str,
    resource: &str,
    subresource: &str,
    namespace: &str,
) -> bool {
    let operation = operation_name(operation);
    let operations = rule.get("operations").and_then(Value::as_array);
    if !list_contains(operations, operation) {
        return false;
    }
    if !list_contains(rule.get("apiGroups").and_then(Value::as_array), group) {
        return false;
    }
    if !list_contains(rule.get("apiVersions").and_then(Value::as_array), version) {
        return false;
    }
    if !rule
        .get("resources")
        .and_then(Value::as_array)
        .is_some_and(|patterns| {
            patterns.iter().any(|pattern| {
                resource_pattern_matches(pattern.as_str().unwrap_or(""), resource, subresource)
            })
        })
    {
        return false;
    }
    match rule.get("scope").and_then(Value::as_str).unwrap_or("*") {
        "*" => true,
        "Namespaced" => !namespace.is_empty(),
        "Cluster" => namespace.is_empty(),
        _ => false,
    }
}

fn operation_name(operation: Operation) -> &'static str {
    match operation {
        Operation::Create => "CREATE",
        Operation::Update => "UPDATE",
        Operation::Delete => "DELETE",
    }
}

fn list_contains(values: Option<&Vec<Value>>, wanted: &str) -> bool {
    values.is_some_and(|values| {
        values
            .iter()
            .any(|value| value.as_str() == Some(wanted) || value.as_str() == Some("*"))
    })
}

fn resource_pattern_matches(pattern: &str, resource: &str, subresource: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    let requested = if subresource.is_empty() {
        resource.to_string()
    } else {
        format!("{resource}/{subresource}")
    };
    pattern == requested
        || (subresource.is_empty() && pattern == resource)
        || (pattern.ends_with("/*") && requested.starts_with(pattern.trim_end_matches('*')))
}

fn selector_is_empty(value: &Value) -> bool {
    value
        .get("matchLabels")
        .and_then(Value::as_object)
        .is_none_or(|labels| labels.is_empty())
        && value
            .get("matchExpressions")
            .and_then(Value::as_array)
            .is_none_or(|expressions| expressions.is_empty())
}

fn selector_matches(
    value: Option<&Value>,
    labels: &BTreeMap<String, String>,
) -> Result<bool, Error> {
    let Some(value) = value else { return Ok(true) };
    if selector_is_empty(value) {
        return Ok(true);
    }
    if let Some(match_labels) = value.get("matchLabels").and_then(Value::as_object) {
        if !match_labels
            .iter()
            .all(|(key, value)| labels.get(key).map(String::as_str) == value.as_str())
        {
            return Ok(false);
        }
    }
    let Some(expressions) = value.get("matchExpressions").and_then(Value::as_array) else {
        return Ok(true);
    };
    for expression in expressions {
        let key = expression.get("key").and_then(Value::as_str).unwrap_or("");
        let values = expression
            .get("values")
            .and_then(Value::as_array)
            .map(|values| values.iter().filter_map(Value::as_str).collect::<Vec<_>>())
            .unwrap_or_default();
        let present = labels.get(key);
        let matches = match expression
            .get("operator")
            .and_then(Value::as_str)
            .unwrap_or("")
        {
            "In" => present.is_some_and(|actual| values.contains(&actual.as_str())),
            "NotIn" => present.is_none_or(|actual| !values.contains(&actual.as_str())),
            "Exists" => present.is_some(),
            "DoesNotExist" => present.is_none(),
            _ => {
                return Err(Error::Invalid {
                    webhook: "selector".to_string(),
                    detail: format!("unsupported selector operator for {key:?}"),
                })
            }
        };
        if !matches {
            return Ok(false);
        }
    }
    Ok(true)
}

enum Invocation {
    Allowed(Value),
    Denied(String),
}

async fn invoke(
    storage: &mut StorageClient,
    webhook: &Value,
    mutating: bool,
    uid: &str,
    operation: Operation,
    group: &str,
    version: &str,
    resource: &str,
    subresource: &str,
    namespace: &str,
    name: &str,
    object: &Value,
    old_object: Option<&Value>,
    identity: Option<&Identity>,
    dry_run: bool,
) -> Result<Invocation, Error> {
    let webhook_name = object_name(webhook);
    let review_version = review_version(webhook, &webhook_name)?;
    let endpoint = endpoint(storage, webhook, &webhook_name).await?;
    let timeout = webhook
        .get("timeoutSeconds")
        .and_then(Value::as_u64)
        .map(|seconds| Duration::from_secs(seconds.clamp(1, 30)))
        .unwrap_or(DEFAULT_TIMEOUT);
    let client = build_client(webhook, endpoint.resolve, timeout, &webhook_name)?;
    let admission_request = json!({
        "uid": uid,
        "kind": {"group": group, "version": version, "kind": object.get("kind").and_then(Value::as_str).unwrap_or("")},
        "resource": {"group": group, "version": version, "resource": resource},
        "requestKind": {"group": group, "version": version, "kind": object.get("kind").and_then(Value::as_str).unwrap_or("")},
        "requestResource": {"group": group, "version": version, "resource": resource},
        "subResource": subresource,
        "name": name,
        "namespace": namespace,
        "operation": operation_name(operation),
        "userInfo": user_info(identity),
        "object": object,
        "oldObject": old_object.filter(|_| operation == Operation::Update).cloned().unwrap_or(Value::Null),
        "dryRun": dry_run,
        "options": {"apiVersion": "meta.k8s.io/v1", "kind": "CreateOptions"}
    });
    let payload = json!({"apiVersion": format!("admission.k8s.io/{review_version}"), "kind": "AdmissionReview", "request": admission_request});
    let response = client
        .post(&endpoint.url)
        .header("Content-Type", "application/json")
        .json(&payload)
        .send()
        .await
        .map_err(|error| Error::Invocation {
            webhook: webhook_name.clone(),
            detail: error.to_string(),
        })?;
    if !response.status().is_success() {
        return Err(Error::Invocation {
            webhook: webhook_name,
            detail: format!("HTTP {}", response.status()),
        });
    }
    let response_body: Value = response.json().await.map_err(|error| Error::Invocation {
        webhook: webhook_name.clone(),
        detail: format!("invalid AdmissionReview response: {error}"),
    })?;
    let admission_response = response_body
        .get("response")
        .ok_or_else(|| Error::Invocation {
            webhook: webhook_name.clone(),
            detail: "response field is missing".to_string(),
        })?;
    if admission_response.get("uid").and_then(Value::as_str) != Some(uid) {
        return Err(Error::Invocation {
            webhook: webhook_name,
            detail: "response UID does not match request UID".to_string(),
        });
    }
    if !admission_response
        .get("allowed")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        let message = admission_response
            .pointer("/status/message")
            .and_then(Value::as_str)
            .unwrap_or("admission webhook denied the request")
            .to_string();
        return Ok(Invocation::Denied(message));
    }
    let mut object = object.clone();
    if let Some(patch) = admission_response.get("patch").and_then(Value::as_str) {
        if !mutating {
            return Err(Error::Invocation {
                webhook: webhook_name,
                detail: "a validating webhook returned a patch".to_string(),
            });
        }
        if admission_response.get("patchType").and_then(Value::as_str) != Some("JSONPatch") {
            return Err(Error::Invocation {
                webhook: webhook_name,
                detail: "only JSONPatch admission responses are supported".to_string(),
            });
        }
        let patch = base64::engine::general_purpose::STANDARD
            .decode(patch)
            .map_err(|error| Error::Invocation {
                webhook: webhook_name.clone(),
                detail: format!("invalid base64 JSONPatch: {error}"),
            })?;
        let patch: Value = serde_json::from_slice(&patch).map_err(|error| Error::Invocation {
            webhook: webhook_name.clone(),
            detail: format!("invalid JSONPatch: {error}"),
        })?;
        crate::patch::json_patch::apply(&mut object, &patch).map_err(|error| {
            Error::Invocation {
                webhook: webhook_name,
                detail: format!("applying JSONPatch: {error}"),
            }
        })?;
    }
    Ok(Invocation::Allowed(object))
}

fn review_version(webhook: &Value, webhook_name: &str) -> Result<&'static str, Error> {
    let versions = webhook
        .get("admissionReviewVersions")
        .and_then(Value::as_array)
        .ok_or_else(|| Error::Invalid {
            webhook: webhook_name.to_string(),
            detail: "admissionReviewVersions is missing".to_string(),
        })?;
    if versions
        .iter()
        .any(|version| version.as_str() == Some("v1"))
    {
        Ok("v1")
    } else if versions
        .iter()
        .any(|version| version.as_str() == Some("v1beta1"))
    {
        Ok("v1beta1")
    } else {
        Err(Error::Invalid {
            webhook: webhook_name.to_string(),
            detail: "no supported AdmissionReview version".to_string(),
        })
    }
}

async fn endpoint(
    storage: &mut StorageClient,
    webhook: &Value,
    webhook_name: &str,
) -> Result<Endpoint, Error> {
    let client_config = webhook.get("clientConfig").ok_or_else(|| Error::Invalid {
        webhook: webhook_name.to_string(),
        detail: "clientConfig is missing".to_string(),
    })?;
    if let Some(url) = client_config.get("url").and_then(Value::as_str) {
        return Ok(Endpoint {
            url: url.to_string(),
            resolve: None,
        });
    }
    let service = client_config.get("service").ok_or_else(|| Error::Invalid {
        webhook: webhook_name.to_string(),
        detail: "clientConfig must contain url or service".to_string(),
    })?;
    let service_name = service.get("name").and_then(Value::as_str).unwrap_or("");
    let service_namespace = service
        .get("namespace")
        .and_then(Value::as_str)
        .unwrap_or("");
    if service_name.is_empty() || service_namespace.is_empty() {
        return Err(Error::Invalid {
            webhook: webhook_name.to_string(),
            detail: "service name and namespace are required".to_string(),
        });
    }
    let service_object = match rest::get(
        storage,
        None,
        "",
        "v1",
        "services",
        Some(service_namespace),
        service_name,
    )
    .await?
    {
        rest::GetOutcome::Found(value) => value,
        rest::GetOutcome::ObjectNotFound | rest::GetOutcome::UnknownResource => {
            return Err(Error::Invocation {
                webhook: webhook_name.to_string(),
                detail: format!("service {service_namespace}/{service_name} was not found"),
            })
        }
    };
    let cluster_ip = service_object
        .pointer("/spec/clusterIP")
        .and_then(Value::as_str)
        .unwrap_or("");
    let ip = cluster_ip
        .parse::<IpAddr>()
        .map_err(|_| Error::Invocation {
            webhook: webhook_name.to_string(),
            detail: format!("service {service_namespace}/{service_name} has no usable ClusterIP"),
        })?;
    let requested_port = service
        .get("port")
        .and_then(Value::as_u64)
        .unwrap_or(DEFAULT_WEBHOOK_PORT as u64) as u16;
    let service_port = service_object
        .pointer("/spec/ports")
        .and_then(Value::as_array)
        .and_then(|ports| {
            ports
                .iter()
                .find(|port| {
                    port.get("port").and_then(Value::as_u64) == Some(requested_port as u64)
                })
                .or_else(|| ports.first())
        })
        .and_then(|port| port.get("port").and_then(Value::as_u64))
        .unwrap_or(requested_port as u64) as u16;
    let host = format!("{service_name}.{service_namespace}.svc");
    let path = client_config
        .get("service")
        .and_then(|service| service.get("path"))
        .and_then(Value::as_str)
        .unwrap_or("/");
    let path = if path.starts_with('/') {
        path.to_string()
    } else {
        format!("/{path}")
    };
    Ok(Endpoint {
        url: format!("https://{host}:{service_port}{path}"),
        resolve: Some((host, SocketAddr::new(ip, service_port))),
    })
}

fn build_client(
    webhook: &Value,
    resolve: Option<(String, SocketAddr)>,
    timeout: Duration,
    webhook_name: &str,
) -> Result<Client, Error> {
    let mut builder = Client::builder().timeout(timeout);
    if let Some(ca_bundle) = webhook
        .pointer("/clientConfig/caBundle")
        .and_then(Value::as_str)
    {
        let ca_bundle = base64::engine::general_purpose::STANDARD
            .decode(ca_bundle)
            .map_err(|error| Error::Invalid {
                webhook: webhook_name.to_string(),
                detail: format!("invalid clientConfig.caBundle: {error}"),
            })?;
        let certificate = Certificate::from_der(&ca_bundle).map_err(|error| Error::Invalid {
            webhook: webhook_name.to_string(),
            detail: format!("invalid clientConfig.caBundle certificate: {error}"),
        })?;
        builder = builder.add_root_certificate(certificate);
    }
    if let Some((host, address)) = resolve {
        builder = builder.resolve(&host, address);
    }
    builder.build().map_err(|error| Error::Invalid {
        webhook: webhook_name.to_string(),
        detail: format!("building webhook HTTP client: {error}"),
    })
}

fn user_info(identity: Option<&Identity>) -> Value {
    match identity {
        Some(identity) => {
            json!({"username": identity.name, "uid": identity.uid, "groups": identity.groups, "extra": {}})
        }
        None => {
            json!({"username": "system:anonymous", "groups": ["system:unauthenticated"], "extra": {}})
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resource_patterns_match_subresources_like_upstream() {
        assert!(resource_pattern_matches("pods", "pods", ""));
        assert!(!resource_pattern_matches("pods", "pods", "status"));
        assert!(resource_pattern_matches("pods/status", "pods", "status"));
        assert!(resource_pattern_matches("pods/*", "pods", "status"));
        assert!(resource_pattern_matches("*", "deployments", "scale"));
    }

    #[test]
    fn webhook_rules_match_core_and_namespaced_requests() {
        let rule = json!({
            "operations": ["CREATE"],
            "apiGroups": [""],
            "apiVersions": ["v1"],
            "resources": ["pods"],
            "scope": "Namespaced"
        });
        assert!(rule_matches(
            &rule,
            Operation::Create,
            "",
            "v1",
            "pods",
            "",
            "default"
        ));
        assert!(!rule_matches(
            &rule,
            Operation::Update,
            "",
            "v1",
            "pods",
            "",
            "default"
        ));
        assert!(!rule_matches(
            &rule,
            Operation::Create,
            "",
            "v1",
            "pods",
            "",
            ""
        ));
    }

    #[test]
    fn selectors_support_match_labels_and_standard_expressions() {
        let selector = json!({
            "matchLabels": {"app": "web"},
            "matchExpressions": [{"key": "tier", "operator": "In", "values": ["frontend"]}]
        });
        let labels = BTreeMap::from([
            (String::from("app"), String::from("web")),
            (String::from("tier"), String::from("frontend")),
        ]);
        assert!(selector_matches(Some(&selector), &labels).unwrap());
    }

    #[test]
    fn match_conditions_filter_a_webhook_before_invocation() {
        let webhook = json!({
            "metadata": {"name": "pod-filter"},
            "rules": [{
                "operations": ["CREATE"],
                "apiGroups": [""],
                "apiVersions": ["v1"],
                "resources": ["pods"],
                "scope": "Namespaced"
            }],
            "matchConditions": [{
                "name": "production-only",
                "expression": "object.metadata.labels.environment == 'production'"
            }]
        });
        let dev_pod = json!({
            "apiVersion": "v1",
            "kind": "Pod",
            "metadata": {"labels": {"environment": "development"}}
        });
        assert!(!matches_webhook(
            &webhook,
            Operation::Create,
            "",
            "v1",
            "pods",
            "",
            "default",
            &dev_pod,
            None,
            None,
            "uid",
            "pod",
            None,
        )
        .unwrap());

        let production_pod = json!({
            "apiVersion": "v1",
            "kind": "Pod",
            "metadata": {"labels": {"environment": "production"}}
        });
        assert!(matches_webhook(
            &webhook,
            Operation::Create,
            "",
            "v1",
            "pods",
            "",
            "default",
            &production_pod,
            None,
            None,
            "uid",
            "pod",
            None,
        )
        .unwrap());
    }
}
