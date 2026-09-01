//! The node authorizer's request-specific restrictions.
//!
//! Real kube-apiserver runs the Node authorizer before RBAC.  A node
//! certificate is identified by the `system:node:<name>` username and the
//! `system:nodes` group.  Most node permissions are ordinary static RBAC
//! rules, but access to pods and the objects a pod consumes is restricted to
//! objects related to that node.  This module keeps that relationship check
//! close to the request path instead of maintaining a second, potentially
//! stale graph cache.
//!
//! The result is deliberately tri-state. `Allow` short-circuits RBAC,
//! `NoOpinion` falls through to the caller's normal RBAC rules, and `Deny`
//! prevents a broad legacy `system:node` binding from bypassing a
//! node-specific relationship check. Body-sensitive operations are completed
//! by the admission-stage `NodeRestriction` plugin after this authorizer has
//! selected the node identity.

use crate::authn::x509::Identity;
use crate::cacher::selector::parse_field_selector;
use crate::server::{path::RequestInfo, rest};
use crate::storage::client::StorageClient;
use serde_json::Value;

const NODE_PREFIX: &str = "system:node:";
const NODES_GROUP: &str = "system:nodes";
const NODE_LEASE_NAMESPACE: &str = "kube-node-lease";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Allow,
    NoOpinion,
    Deny,
}

/// Applies the Node authorizer to one parsed request.
pub async fn authorize(
    storage: &mut StorageClient,
    identity: Option<&Identity>,
    info: &RequestInfo,
) -> Result<Decision, String> {
    let Some(node_name) = node_name(identity) else {
        return Ok(Decision::NoOpinion);
    };

    if !info.is_resource_request {
        return Ok(Decision::NoOpinion);
    }

    let decision = match (info.api_group.as_str(), info.resource.as_str()) {
        ("", "nodes") => node_access(node_name, info),
        ("", "pods") => pod_access(storage, node_name, info).await?,
        ("", "secrets" | "configmaps") => {
            named_pod_reference_access(storage, node_name, info).await?
        }
        ("", "persistentvolumeclaims") => pvc_access(storage, node_name, info).await?,
        ("", "persistentvolumes") => pv_access(storage, node_name, info).await?,
        ("", "serviceaccounts") => service_account_access(storage, node_name, info).await?,
        ("coordination.k8s.io", "leases") => lease_access(node_name, info),
        ("storage.k8s.io", "csinodes") => csi_node_access(node_name, info),
        ("storage.k8s.io", "volumeattachments") => {
            volume_attachment_access(storage, node_name, info).await?
        }
        ("resource.k8s.io", "resourceslices") => {
            resource_slice_access(storage, node_name, info).await?
        }
        ("resource.k8s.io", "resourceclaims") => {
            resource_claim_access(storage, node_name, info).await?
        }
        ("authentication.k8s.io", "tokenreviews")
            if info.verb == "create" && info.subresource.is_empty() =>
        {
            Decision::Allow
        }
        _ => Decision::NoOpinion,
    };
    Ok(decision)
}

/// Returns the node name only for the identity convention used by the real
/// x509 node authenticator. A username with the prefix but without the
/// `system:nodes` group is an ordinary user, not a node.
pub fn node_name(identity: Option<&Identity>) -> Option<&str> {
    let identity = identity?;
    let node_name = identity.name.strip_prefix(NODE_PREFIX)?;
    if node_name.is_empty() || !identity.groups.iter().any(|group| group == NODES_GROUP) {
        return None;
    }
    Some(node_name)
}

fn node_access(node_name: &str, info: &RequestInfo) -> Decision {
    if !info.subresource.is_empty() && info.subresource != "status" {
        return Decision::NoOpinion;
    }
    match (info.subresource.as_str(), info.verb.as_str()) {
        // NodeRestriction validates the submitted object name and body after
        // this authorizer grants the body-sensitive create.
        ("", "create") => Decision::Allow,
        ("", "get" | "list" | "watch") if info.name == node_name => Decision::Allow,
        ("", "update" | "patch") if info.name == node_name => Decision::Allow,
        ("status", "update" | "patch") if info.name == node_name => Decision::Allow,
        _ => Decision::Deny,
    }
}

async fn pod_access(
    storage: &mut StorageClient,
    node_name: &str,
    info: &RequestInfo,
) -> Result<Decision, String> {
    if info.subresource == "status" {
        return if matches!(info.verb.as_str(), "update" | "patch") && !info.name.is_empty() {
            related_pod(
                storage,
                node_name,
                info.namespace.as_str(),
                info.name.as_str(),
            )
            .await
            .map(|related| {
                if related {
                    Decision::Allow
                } else {
                    Decision::Deny
                }
            })
        } else {
            Ok(Decision::Deny)
        };
    }
    if info.subresource == "eviction" && info.verb == "create" {
        return Ok(Decision::NoOpinion);
    }
    if !info.subresource.is_empty() {
        return Ok(Decision::NoOpinion);
    }
    match info.verb.as_str() {
        "list" | "watch"
            if has_exact_field_selector(&info.field_selector, "spec.nodeName", node_name) =>
        {
            Ok(Decision::Allow)
        }
        "get" | "list" | "watch" if !info.name.is_empty() => related_pod(
            storage,
            node_name,
            info.namespace.as_str(),
            info.name.as_str(),
        )
        .await
        .map(|related| {
            if related {
                Decision::Allow
            } else {
                Decision::Deny
            }
        }),
        // NodeRestriction checks that creates/deletes are mirror Pods owned by
        // this node; the authorizer must allow them to reach that plugin.
        "create" | "delete" => Ok(Decision::Allow),
        _ => Ok(Decision::Deny),
    }
}

async fn named_pod_reference_access(
    storage: &mut StorageClient,
    node_name: &str,
    info: &RequestInfo,
) -> Result<Decision, String> {
    if !matches!(info.verb.as_str(), "get" | "list" | "watch")
        || !info.subresource.is_empty()
        || info.namespace.is_empty()
        || info.name.is_empty()
    {
        return Ok(Decision::Deny);
    }
    let pods = pods_on_node(storage, node_name, Some(info.namespace.as_str())).await?;
    let related = pods
        .iter()
        .any(|pod| pod_references(pod, info.resource.as_str(), info.name.as_str()));
    Ok(if related {
        Decision::Allow
    } else {
        Decision::Deny
    })
}

async fn pvc_access(
    storage: &mut StorageClient,
    node_name: &str,
    info: &RequestInfo,
) -> Result<Decision, String> {
    if info.namespace.is_empty() || info.name.is_empty() {
        return Ok(Decision::Deny);
    }
    if info.subresource == "status" {
        if !matches!(info.verb.as_str(), "update" | "patch") {
            return Ok(Decision::Deny);
        }
    } else if info.subresource.is_empty() && info.verb == "get" {
        // allowed
    } else {
        return Ok(Decision::Deny);
    }
    let pods = pods_on_node(storage, node_name, Some(info.namespace.as_str())).await?;
    Ok(
        if pods
            .iter()
            .any(|pod| pod_references(pod, "persistentvolumeclaims", info.name.as_str()))
        {
            Decision::Allow
        } else {
            Decision::Deny
        },
    )
}

async fn pv_access(
    storage: &mut StorageClient,
    node_name: &str,
    info: &RequestInfo,
) -> Result<Decision, String> {
    if info.verb != "get" || !info.subresource.is_empty() || info.name.is_empty() {
        return Ok(Decision::Deny);
    }
    let pods = pods_on_node(storage, node_name, None).await?;
    for pod in pods {
        let namespace = pod_namespace(&pod);
        for pvc_name in referenced_names(&pod, "persistentvolumeclaims") {
            let pvc = match rest::get(
                storage,
                None,
                "",
                "v1",
                "persistentvolumeclaims",
                Some(namespace.as_str()),
                &pvc_name,
            )
            .await
            {
                Ok(rest::GetOutcome::Found(pvc)) => pvc,
                Ok(rest::GetOutcome::ObjectNotFound | rest::GetOutcome::UnknownResource) => {
                    continue
                }
                Err(error) => {
                    return Err(format!(
                        "resolving PVC {namespace}/{pvc_name} for node authorization: {error}"
                    ))
                }
            };
            if pvc.pointer("/spec/volumeName").and_then(Value::as_str) == Some(info.name.as_str()) {
                return Ok(Decision::Allow);
            }
        }
    }
    Ok(Decision::Deny)
}

async fn service_account_access(
    storage: &mut StorageClient,
    node_name: &str,
    info: &RequestInfo,
) -> Result<Decision, String> {
    if info.subresource != "token"
        || info.verb != "create"
        || info.namespace.is_empty()
        || info.name.is_empty()
    {
        return Ok(Decision::Deny);
    }
    let pods = pods_on_node(storage, node_name, Some(info.namespace.as_str())).await?;
    let related = pods.iter().any(|pod| pod_service_account(pod) == info.name);
    Ok(if related {
        Decision::Allow
    } else {
        Decision::Deny
    })
}

fn lease_access(node_name: &str, info: &RequestInfo) -> Decision {
    if !info.subresource.is_empty()
        || info.namespace != NODE_LEASE_NAMESPACE
        || !matches!(
            info.verb.as_str(),
            "get" | "create" | "update" | "patch" | "delete"
        )
    {
        return Decision::Deny;
    }
    if info.verb == "create" || info.name == node_name {
        Decision::Allow
    } else {
        Decision::Deny
    }
}

fn csi_node_access(node_name: &str, info: &RequestInfo) -> Decision {
    let valid_subresource = info.subresource.is_empty() || info.subresource == "status";
    let valid_verb = if info.subresource == "status" {
        matches!(info.verb.as_str(), "get" | "update" | "patch")
    } else {
        matches!(
            info.verb.as_str(),
            "get" | "create" | "update" | "patch" | "delete"
        )
    };
    if valid_subresource && valid_verb && (info.verb == "create" || info.name == node_name) {
        Decision::Allow
    } else {
        Decision::Deny
    }
}

async fn volume_attachment_access(
    storage: &mut StorageClient,
    node_name: &str,
    info: &RequestInfo,
) -> Result<Decision, String> {
    if info.verb != "get" || !info.subresource.is_empty() || info.name.is_empty() {
        return Ok(Decision::Deny);
    }
    object_node_name(
        storage,
        "storage.k8s.io",
        "v1",
        "volumeattachments",
        info.name.as_str(),
    )
    .await
    .map(|name| {
        if name.as_deref() == Some(node_name) {
            Decision::Allow
        } else {
            Decision::Deny
        }
    })
}

async fn resource_slice_access(
    storage: &mut StorageClient,
    node_name: &str,
    info: &RequestInfo,
) -> Result<Decision, String> {
    if !info.subresource.is_empty() {
        return Ok(Decision::Deny);
    }
    match info.verb.as_str() {
        "list" | "watch" | "deletecollection" => Ok(
            if has_exact_field_selector(&info.field_selector, "spec.nodeName", node_name) {
                Decision::Allow
            } else {
                Decision::Deny
            },
        ),
        "get" | "update" | "patch" | "delete" if !info.name.is_empty() => object_node_name(
            storage,
            "resource.k8s.io",
            "v1",
            "resourceslices",
            info.name.as_str(),
        )
        .await
        .map(|name| {
            if name.as_deref() == Some(node_name) {
                Decision::Allow
            } else {
                Decision::Deny
            }
        }),
        // NodeRestriction checks the NodeName in a newly-created object.
        "create" => Ok(Decision::NoOpinion),
        _ => Ok(Decision::Deny),
    }
}

async fn resource_claim_access(
    storage: &mut StorageClient,
    node_name: &str,
    info: &RequestInfo,
) -> Result<Decision, String> {
    if info.verb != "get"
        || !info.subresource.is_empty()
        || info.namespace.is_empty()
        || info.name.is_empty()
    {
        return Ok(Decision::Deny);
    }
    let pods = pods_on_node(storage, node_name, Some(info.namespace.as_str())).await?;
    Ok(
        if pods
            .iter()
            .any(|pod| pod_references(pod, "resourceclaims", info.name.as_str()))
        {
            Decision::Allow
        } else {
            Decision::Deny
        },
    )
}

async fn related_pod(
    storage: &mut StorageClient,
    node_name: &str,
    namespace: &str,
    name: &str,
) -> Result<bool, String> {
    let pod = match rest::get(storage, None, "", "v1", "pods", Some(namespace), name).await {
        Ok(rest::GetOutcome::Found(pod)) => pod,
        Ok(rest::GetOutcome::ObjectNotFound | rest::GetOutcome::UnknownResource) => {
            return Ok(false)
        }
        Err(error) => {
            return Err(format!(
                "resolving pod {namespace}/{name} for node authorization: {error}"
            ))
        }
    };
    Ok(pod.pointer("/spec/nodeName").and_then(Value::as_str) == Some(node_name))
}

async fn pods_on_node(
    storage: &mut StorageClient,
    node_name: &str,
    namespace: Option<&str>,
) -> Result<Vec<Value>, String> {
    let list = match rest::list(storage, None, "", "v1", "pods", namespace, "", "", 0, "").await {
        Ok(rest::ListOutcome::Found(list)) => list,
        Ok(rest::ListOutcome::UnknownResource | rest::ListOutcome::InvalidContinueToken) => {
            return Ok(Vec::new())
        }
        Err(error) => return Err(format!("listing pods for node authorization: {error}")),
    };
    Ok(list
        .get("items")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|pod| pod.pointer("/spec/nodeName").and_then(Value::as_str) == Some(node_name))
        .cloned()
        .collect())
}

async fn object_node_name(
    storage: &mut StorageClient,
    group: &str,
    version: &str,
    resource: &str,
    name: &str,
) -> Result<Option<String>, String> {
    match rest::get(storage, None, group, version, resource, None, name).await {
        Ok(rest::GetOutcome::Found(object)) => Ok(object
            .pointer("/spec/nodeName")
            .and_then(Value::as_str)
            .map(str::to_string)),
        Ok(rest::GetOutcome::ObjectNotFound | rest::GetOutcome::UnknownResource) => Ok(None),
        Err(error) => Err(format!(
            "resolving {group}/{resource}/{name} for node authorization: {error}"
        )),
    }
}

fn has_exact_field_selector(selector: &str, field: &str, value: &str) -> bool {
    parse_field_selector(selector)
        .ok()
        .is_some_and(|requirements| {
            requirements.iter().any(|requirement| {
                requirement.field == field && !requirement.negated && requirement.value == value
            })
        })
}

fn pod_namespace(pod: &Value) -> String {
    pod.pointer("/metadata/namespace")
        .and_then(Value::as_str)
        .unwrap_or("default")
        .to_string()
}

fn pod_service_account(pod: &Value) -> String {
    pod.pointer("/spec/serviceAccountName")
        .and_then(Value::as_str)
        .unwrap_or("default")
        .to_string()
}

fn pod_references(pod: &Value, resource: &str, name: &str) -> bool {
    referenced_names(pod, resource)
        .iter()
        .any(|candidate| candidate == name)
}

fn referenced_names(pod: &Value, resource: &str) -> Vec<String> {
    let mut names = Vec::new();
    let spec = pod.get("spec").unwrap_or(&Value::Null);
    if resource == "persistentvolumeclaims" {
        if let Some(volumes) = spec.get("volumes").and_then(Value::as_array) {
            names.extend(volumes.iter().filter_map(|volume| {
                volume
                    .pointer("/persistentVolumeClaim/claimName")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            }));
        }
    } else if resource == "resourceclaims" {
        if let Some(claims) = spec.get("resourceClaims").and_then(Value::as_array) {
            names.extend(claims.iter().filter_map(|claim| {
                claim
                    .get("name")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            }));
        }
    } else if let Some(volumes) = spec.get("volumes").and_then(Value::as_array) {
        let key = match resource {
            "secrets" => "secret",
            "configmaps" => "configMap",
            _ => "",
        };
        if !key.is_empty() {
            names.extend(volumes.iter().filter_map(|volume| {
                volume
                    .pointer(&format!("/{key}/secretName"))
                    .or_else(|| volume.pointer(&format!("/{key}/name")))
                    .and_then(Value::as_str)
                    .map(str::to_string)
            }));
        }
    }
    if resource == "secrets" || resource == "configmaps" {
        let key = if resource == "secrets" {
            "secret"
        } else {
            "configMap"
        };
        for pull in spec
            .get("imagePullSecrets")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if resource == "secrets" {
                if let Some(name) = pull.get("name").and_then(Value::as_str) {
                    names.push(name.to_string());
                }
            }
        }
        for container in spec
            .get("containers")
            .into_iter()
            .chain(spec.get("initContainers"))
            .chain(spec.get("ephemeralContainers"))
            .flat_map(|containers| containers.as_array().into_iter().flatten())
        {
            for env_from in container
                .get("envFrom")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                if let Some(name) = env_from
                    .pointer(&format!("/{key}Ref/name"))
                    .and_then(Value::as_str)
                {
                    names.push(name.to_string());
                }
            }
            for env in container
                .get("env")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                if let Some(name) = env
                    .pointer(&format!("/valueFrom/{key}KeyRef/name"))
                    .and_then(Value::as_str)
                {
                    names.push(name.to_string());
                }
            }
        }
    }
    names
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity(name: &str, groups: &[&str]) -> Identity {
        Identity {
            name: name.to_string(),
            groups: groups.iter().map(|group| (*group).to_string()).collect(),
            uid: None,
            extra: Default::default(),
            credential_id: (String::new(), Vec::new()),
        }
    }

    fn info(verb: &str, resource: &str, name: &str) -> RequestInfo {
        RequestInfo {
            is_resource_request: true,
            verb: verb.to_string(),
            api_group: String::new(),
            api_version: "v1".to_string(),
            resource: resource.to_string(),
            name: name.to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn only_a_system_nodes_identity_is_a_node() {
        assert_eq!(
            node_name(Some(&identity("system:node:worker-1", &["system:nodes"]))),
            Some("worker-1")
        );
        assert_eq!(
            node_name(Some(&identity("system:node:worker-1", &["devs"]))),
            None
        );
        assert_eq!(node_name(Some(&identity("alice", &["system:nodes"]))), None);
    }

    #[test]
    fn a_node_can_only_read_its_own_node_object() {
        let own = info("get", "nodes", "worker-1");
        let other = info("get", "nodes", "worker-2");
        assert_eq!(node_access("worker-1", &own), Decision::Allow);
        assert_eq!(node_access("worker-1", &other), Decision::Deny);
    }

    #[test]
    fn body_sensitive_node_operations_are_left_for_node_restriction() {
        // NodeRestriction admits only a node object whose body names this
        // node, so the body-independent authorizer decision must be Allow.
        assert_eq!(
            node_access("worker-1", &info("create", "nodes", "")),
            Decision::Allow
        );
    }

    #[test]
    fn pod_list_requires_an_exact_node_field_selector() {
        let mut scoped = info("list", "pods", "");
        scoped.field_selector = "spec.nodeName=worker-1".to_string();
        assert!(has_exact_field_selector(
            &scoped.field_selector,
            "spec.nodeName",
            "worker-1"
        ));
        scoped.field_selector = "spec.nodeName!=worker-1".to_string();
        assert!(!has_exact_field_selector(
            &scoped.field_selector,
            "spec.nodeName",
            "worker-1"
        ));
    }

    #[test]
    fn pod_secret_and_volume_references_are_seen() {
        let pod = serde_json::json!({
            "spec": {
                "volumes": [{"secret": {"secretName": "pull-secret"}}, {"configMap": {"name": "cluster-ca"}}, {"persistentVolumeClaim": {"claimName": "data"}}],
                "containers": [{"env": [{"valueFrom": {"secretKeyRef": {"name": "env-secret"}}}]}]
            }
        });
        assert!(pod_references(&pod, "secrets", "pull-secret"));
        assert!(pod_references(&pod, "secrets", "env-secret"));
        assert!(pod_references(&pod, "configmaps", "cluster-ca"));
        assert!(pod_references(&pod, "persistentvolumeclaims", "data"));
        assert!(!pod_references(&pod, "secrets", "other"));
    }
}
