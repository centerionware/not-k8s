//! `NodeRestriction` admission for node identities.
//!
//! The Node authorizer can decide which stored objects a node may address, but
//! it cannot inspect a request body. Real kube-apiserver therefore follows it
//! with this admission plugin, which constrains body-sensitive operations such
//! as node label changes and mirror-pod creation. This module keeps those
//! checks in admission, where both the candidate and old object are available.
//!
//! The implementation follows the resource rules in Kubernetes 1.34's
//! `plugin/pkg/admission/noderestriction/admission.go` for the resource types
//! this server currently exposes. Unknown resources are deliberately ignored;
//! the ordinary authorizer and their own admission/validation remain the
//! authorities for those resources.

use crate::admission::attributes::Operation;
use crate::authn::x509::Identity;
use crate::authz::node::node_name;
use crate::server::rest::{self, GetOutcome};
use crate::storage::client::StorageClient;
use serde_json::Value;
use std::collections::BTreeSet;

const NODE_LEASE_NAMESPACE: &str = "kube-node-lease";
const MIRROR_POD_ANNOTATION: &str = "kubernetes.io/config.mirror";

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("node restriction denied the request: {0}")]
    Forbidden(String),
    #[error("node restriction lookup failed: {0}")]
    Lookup(String),
}

/// Applies body-sensitive restrictions for a node identity.
///
/// `object` is the submitted object (or the prepared post-patch candidate),
/// and `old_object` is the currently stored object when the operation has one.
/// Non-node identities and unrelated resource types are allowed through.
pub async fn validate(
    storage: &mut StorageClient,
    identity: Option<&Identity>,
    operation: Operation,
    group: &str,
    resource: &str,
    subresource: &str,
    namespace: &str,
    name: &str,
    object: Option<&Value>,
    old_object: Option<&Value>,
) -> Result<(), Error> {
    let Some(node_name) = node_name(identity) else {
        return Ok(());
    };

    match (group, resource, subresource) {
        ("", "nodes", "" | "status") => {
            validate_node(node_name, operation, name, object, old_object)
        }
        ("", "pods", "") => {
            validate_pod(
                storage, node_name, operation, namespace, name, object, old_object,
            )
            .await
        }
        ("", "pods", "status") => validate_pod_status(node_name, operation, object, old_object),
        ("", "pods", "eviction") => {
            validate_pod_eviction(storage, node_name, operation, namespace, name, object).await
        }
        ("", "serviceaccounts", "token") => {
            validate_service_account_token(storage, node_name, operation, namespace, object).await
        }
        ("coordination.k8s.io", "leases", "") => {
            validate_lease(node_name, operation, namespace, name, object)
        }
        ("storage.k8s.io", "csinodes", "") => {
            validate_same_named_object(node_name, operation, name, object)
        }
        ("resource.k8s.io", "resourceslices", "") => {
            validate_resource_slice(node_name, operation, object, old_object)
        }
        _ => Ok(()),
    }
}

fn validate_node(
    node_name: &str,
    operation: Operation,
    request_name: &str,
    object: Option<&Value>,
    old_object: Option<&Value>,
) -> Result<(), Error> {
    if !request_name.is_empty() && request_name != node_name {
        return forbidden(format!(
            "node {node_name:?} is not allowed to modify node {request_name:?}"
        ));
    }

    if operation == Operation::Delete {
        return Ok(());
    }
    let object =
        object.ok_or_else(|| Error::Forbidden("node request has no object".to_string()))?;
    if operation == Operation::Create {
        if metadata_name(object) != Some(node_name) {
            return forbidden(format!(
                "node {node_name:?} may only create its own Node object"
            ));
        }
        if object
            .pointer("/spec/configSource")
            .is_some_and(|value| !value.is_null())
        {
            return forbidden(format!(
                "node {node_name:?} is not allowed to create a Node with configSource"
            ));
        }
        return validate_node_labels(node_name, object, None);
    }

    if operation == Operation::Update {
        let old_object = old_object
            .ok_or_else(|| Error::Forbidden("node update has no existing object".to_string()))?;
        let new_config_source = object.pointer("/spec/configSource").unwrap_or(&Value::Null);
        let old_config_source = old_object
            .pointer("/spec/configSource")
            .unwrap_or(&Value::Null);
        if new_config_source != old_config_source && !new_config_source.is_null() {
            return forbidden(format!(
                "node {node_name:?} is not allowed to update configSource"
            ));
        }
        if object.pointer("/spec/taints") != old_object.pointer("/spec/taints") {
            return forbidden(format!(
                "node {node_name:?} is not allowed to modify taints"
            ));
        }
        if object.pointer("/metadata/ownerReferences")
            != old_object.pointer("/metadata/ownerReferences")
        {
            return forbidden(format!(
                "node {node_name:?} is not allowed to modify ownerReferences"
            ));
        }
        validate_node_labels(node_name, object, Some(old_object))?;
    }

    Ok(())
}

fn validate_node_labels(
    node_name: &str,
    object: &Value,
    old_object: Option<&Value>,
) -> Result<(), Error> {
    let new_labels = labels(object);
    let old_labels = old_object.map(labels).unwrap_or_default();
    let modified = new_labels
        .keys()
        .chain(old_labels.keys())
        .filter(|key| new_labels.get(*key) != old_labels.get(*key))
        .cloned()
        .collect::<BTreeSet<_>>();

    let forbidden_labels = modified
        .into_iter()
        .filter(|key| is_forbidden_label(key))
        .collect::<Vec<_>>();
    if forbidden_labels.is_empty() {
        Ok(())
    } else {
        forbidden(format!(
            "node {node_name:?} is not allowed to modify labels: {}",
            forbidden_labels.join(", ")
        ))
    }
}

fn is_forbidden_label(key: &str) -> bool {
    // These are the stable and legacy topology/platform labels the upstream
    // kubelet label helper permits a node to manage. Other reserved
    // `kubernetes.io`/`k8s.io` labels stay protected from node identities.
    if matches!(
        key,
        "kubernetes.io/hostname"
            | "kubernetes.io/os"
            | "kubernetes.io/arch"
            | "beta.kubernetes.io/os"
            | "beta.kubernetes.io/arch"
            | "beta.kubernetes.io/instance-type"
            | "node.kubernetes.io/instance-type"
            | "topology.kubernetes.io/zone"
            | "topology.kubernetes.io/region"
            | "failure-domain.beta.kubernetes.io/zone"
            | "failure-domain.beta.kubernetes.io/region"
    ) {
        return false;
    }
    let namespace = key
        .split_once('/')
        .map(|(namespace, _)| namespace)
        .unwrap_or("");
    namespace == "node-restriction.kubernetes.io"
        || namespace.ends_with(".node-restriction.kubernetes.io")
        || namespace == "kubernetes.io"
        || namespace.ends_with(".kubernetes.io")
        || namespace == "k8s.io"
        || namespace.ends_with(".k8s.io")
}

async fn validate_pod(
    storage: &mut StorageClient,
    node_name: &str,
    operation: Operation,
    namespace: &str,
    name: &str,
    object: Option<&Value>,
    _old_object: Option<&Value>,
) -> Result<(), Error> {
    match operation {
        Operation::Create => {
            let pod = required_object(object)?;
            if !pod
                .pointer("/metadata/annotations")
                .and_then(|annotations| annotations.get(MIRROR_POD_ANNOTATION))
                .is_some()
            {
                return forbidden(format!("node {node_name:?} may only create mirror Pods"));
            }
            if pod.pointer("/spec/nodeName").and_then(Value::as_str) != Some(node_name) {
                return forbidden(format!(
                    "node {node_name:?} may only create Pods assigned to itself"
                ));
            }
            let owners = pod
                .pointer("/metadata/ownerReferences")
                .and_then(Value::as_array);
            if owners.map_or(true, |owners| owners.len() != 1) {
                return forbidden(format!(
                    "node {node_name:?} may only create mirror Pods owned by itself"
                ));
            }
            let owner = &owners.expect("checked above")[0];
            let owner_matches = owner.get("apiVersion").and_then(Value::as_str) == Some("v1")
                && owner.get("kind").and_then(Value::as_str) == Some("Node")
                && owner.get("name").and_then(Value::as_str) == Some(node_name)
                && owner.get("controller").and_then(Value::as_bool) == Some(true)
                && owner.get("blockOwnerDeletion").and_then(Value::as_bool) != Some(true);
            if !owner_matches {
                return forbidden(format!(
                    "node {node_name:?} may only create mirror Pods with itself as controller owner"
                ));
            }
            let node = get_required(storage, "", "v1", "nodes", None, node_name).await?;
            if owner.get("uid") != node.pointer("/metadata/uid") {
                return forbidden(format!(
                    "node {node_name:?} mirror Pod owner UID does not match the Node UID"
                ));
            }
            if references_api_objects(pod) {
                return forbidden(format!(
                    "node {node_name:?} may not create mirror Pods that reference API objects"
                ));
            }
            Ok(())
        }
        Operation::Delete => {
            let pod = get_required(storage, "", "v1", "pods", Some(namespace), name).await?;
            if pod.pointer("/spec/nodeName").and_then(Value::as_str) == Some(node_name) {
                Ok(())
            } else {
                forbidden(format!(
                    "node {node_name:?} may only delete Pods assigned to itself"
                ))
            }
        }
        Operation::Update => Ok(()),
    }
}

fn validate_pod_status(
    node_name: &str,
    operation: Operation,
    object: Option<&Value>,
    old_object: Option<&Value>,
) -> Result<(), Error> {
    if operation != Operation::Update {
        return forbidden("Pod status may only be updated".to_string());
    }
    let pod = required_object(object)?;
    let old_pod = old_object
        .ok_or_else(|| Error::Forbidden("Pod status update has no existing Pod".to_string()))?;
    if old_pod.pointer("/spec/nodeName").and_then(Value::as_str) != Some(node_name) {
        return forbidden(format!(
            "node {node_name:?} may only update status for Pods assigned to itself"
        ));
    }
    if pod.pointer("/metadata/labels") != old_pod.pointer("/metadata/labels") {
        return forbidden(format!(
            "node {node_name:?} may not update Pod labels through the status subresource"
        ));
    }
    if pod.pointer("/status/resourceClaimStatuses")
        != old_pod.pointer("/status/resourceClaimStatuses")
        || pod.pointer("/status/extendedResourceClaimStatus")
            != old_pod.pointer("/status/extendedResourceClaimStatus")
    {
        return forbidden(format!(
            "node {node_name:?} may not change Pod resource claim status through the status subresource"
        ));
    }
    Ok(())
}

async fn validate_pod_eviction(
    storage: &mut StorageClient,
    node_name: &str,
    operation: Operation,
    namespace: &str,
    name: &str,
    object: Option<&Value>,
) -> Result<(), Error> {
    if operation != Operation::Create {
        return forbidden("Pod eviction must be created".to_string());
    }
    let eviction_name = if name.is_empty() {
        object.and_then(metadata_name).unwrap_or("")
    } else {
        name
    };
    if eviction_name.is_empty() {
        return forbidden("Pod eviction did not identify a Pod".to_string());
    }
    let pod = get_required(storage, "", "v1", "pods", Some(namespace), eviction_name).await?;
    if pod.pointer("/spec/nodeName").and_then(Value::as_str) == Some(node_name) {
        Ok(())
    } else {
        forbidden(format!(
            "node {node_name:?} may only evict Pods assigned to itself"
        ))
    }
}

async fn validate_service_account_token(
    storage: &mut StorageClient,
    node_name: &str,
    operation: Operation,
    namespace: &str,
    object: Option<&Value>,
) -> Result<(), Error> {
    if operation != Operation::Create {
        return Ok(());
    }
    let request = required_object(object)?;
    let bound = request.pointer("/spec/boundObjectRef");
    let valid_ref = bound
        .and_then(|value| value.get("apiVersion"))
        .and_then(Value::as_str)
        == Some("v1")
        && bound
            .and_then(|value| value.get("kind"))
            .and_then(Value::as_str)
            == Some("Pod")
        && bound
            .and_then(|value| value.get("name"))
            .and_then(Value::as_str)
            .is_some_and(|name| !name.is_empty())
        && bound
            .and_then(|value| value.get("uid"))
            .and_then(Value::as_str)
            .is_some_and(|uid| !uid.is_empty());
    if !valid_ref {
        return forbidden("node requested a token not bound to a Pod".to_string());
    }
    let pod_name = bound
        .and_then(|value| value.get("name"))
        .and_then(Value::as_str)
        .expect("validated bound Pod name");
    let pod = get_required(storage, "", "v1", "pods", Some(namespace), pod_name).await?;
    if bound.and_then(|value| value.get("uid")) != pod.pointer("/metadata/uid") {
        return forbidden("the token boundObjectRef UID does not match the Pod UID".to_string());
    }
    if pod.pointer("/spec/nodeName").and_then(Value::as_str) != Some(node_name) {
        return forbidden("the token is bound to a Pod scheduled on another node".to_string());
    }
    Ok(())
}

fn validate_lease(
    node_name: &str,
    operation: Operation,
    namespace: &str,
    request_name: &str,
    object: Option<&Value>,
) -> Result<(), Error> {
    if namespace != NODE_LEASE_NAMESPACE {
        return forbidden(format!(
            "node leases may only be accessed in {NODE_LEASE_NAMESPACE:?}"
        ));
    }
    let lease_name = if operation == Operation::Create {
        object.and_then(metadata_name).unwrap_or("")
    } else {
        request_name
    };
    if lease_name == node_name {
        Ok(())
    } else {
        forbidden(format!(
            "node {node_name:?} may only access its own Node lease"
        ))
    }
}

fn validate_same_named_object(
    node_name: &str,
    operation: Operation,
    request_name: &str,
    object: Option<&Value>,
) -> Result<(), Error> {
    let object_name = if operation == Operation::Create {
        object.and_then(metadata_name).unwrap_or("")
    } else {
        request_name
    };
    if object_name == node_name {
        Ok(())
    } else {
        forbidden(format!(
            "node {node_name:?} may only access the object named after itself"
        ))
    }
}

fn validate_resource_slice(
    node_name: &str,
    operation: Operation,
    object: Option<&Value>,
    old_object: Option<&Value>,
) -> Result<(), Error> {
    if !matches!(operation, Operation::Create | Operation::Delete) {
        return Ok(());
    }
    let object = if operation == Operation::Create {
        required_object(object)?
    } else {
        old_object
            .ok_or_else(|| Error::Forbidden("ResourceSlice delete has no old object".to_string()))?
    };
    if object.pointer("/spec/nodeName").and_then(Value::as_str) == Some(node_name) {
        Ok(())
    } else {
        forbidden(format!(
            "ResourceSlice must have spec.nodeName set to {node_name:?}"
        ))
    }
}

fn references_api_objects(pod: &Value) -> bool {
    let Some(spec) = pod.get("spec") else {
        return false;
    };
    if spec
        .get("serviceAccountName")
        .and_then(Value::as_str)
        .is_some_and(|name| !name.is_empty())
        || spec
            .get("imagePullSecrets")
            .and_then(Value::as_array)
            .is_some_and(|items| !items.is_empty())
    {
        return true;
    }
    for volume in spec
        .get("volumes")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        if volume.get("secret").is_some()
            || volume.get("configMap").is_some()
            || volume.get("persistentVolumeClaim").is_some()
        {
            return true;
        }
        if volume
            .pointer("/projected/sources")
            .and_then(Value::as_array)
            .is_some_and(|sources| {
                sources.iter().any(|source| {
                    source.get("secret").is_some()
                        || source.get("configMap").is_some()
                        || source.get("serviceAccountToken").is_some()
                })
            })
        {
            return true;
        }
    }
    for container in spec
        .get("initContainers")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .chain(
            spec.get("containers")
                .and_then(Value::as_array)
                .into_iter()
                .flatten(),
        )
    {
        if container
            .get("env")
            .and_then(Value::as_array)
            .is_some_and(|env| {
                env.iter().any(|item| {
                    item.pointer("/valueFrom/secretKeyRef").is_some()
                        || item.pointer("/valueFrom/configMapKeyRef").is_some()
                        || item.pointer("/valueFrom/fieldRef").is_some()
                        || item.pointer("/valueFrom/resourceFieldRef").is_some()
                })
            })
            || container
                .get("envFrom")
                .and_then(Value::as_array)
                .is_some_and(|env| {
                    env.iter().any(|item| {
                        item.get("secretRef").is_some() || item.get("configMapRef").is_some()
                    })
                })
        {
            return true;
        }
    }
    false
}

fn labels(object: &Value) -> std::collections::BTreeMap<String, Value> {
    object
        .pointer("/metadata/labels")
        .and_then(Value::as_object)
        .map(|labels| {
            labels
                .iter()
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect()
        })
        .unwrap_or_default()
}

fn metadata_name(object: &Value) -> Option<&str> {
    object.pointer("/metadata/name").and_then(Value::as_str)
}

fn required_object(object: Option<&Value>) -> Result<&Value, Error> {
    object.ok_or_else(|| Error::Forbidden("request has no object".to_string()))
}

async fn get_required(
    storage: &mut StorageClient,
    group: &str,
    version: &str,
    resource: &str,
    namespace: Option<&str>,
    name: &str,
) -> Result<Value, Error> {
    match rest::get(storage, None, group, version, resource, namespace, name).await {
        Ok(GetOutcome::Found(object)) => Ok(object),
        Ok(GetOutcome::ObjectNotFound | GetOutcome::UnknownResource) => Err(Error::Forbidden(
            format!("the related {resource}/{name} object was not found"),
        )),
        Err(error) => Err(Error::Lookup(error.to_string())),
    }
}

fn forbidden<T>(message: String) -> Result<T, Error> {
    Err(Error::Forbidden(message))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::authn::x509::Identity;
    use serde_json::json;

    fn node_identity() -> Identity {
        Identity {
            name: "system:node:worker-1".to_string(),
            groups: vec!["system:nodes".to_string()],
            uid: None,
            credential_id: (String::new(), Vec::new()),
        }
    }

    #[test]
    fn node_identity_cannot_change_restriction_labels() {
        let old = json!({
            "metadata": {"name": "worker-1", "labels": {"kubernetes.io/hostname": "worker-1"}},
            "spec": {"taints": []}
        });
        let new = json!({
            "metadata": {"name": "worker-1", "labels": {
                "kubernetes.io/hostname": "worker-1",
                "node-restriction.kubernetes.io/pwned": "true"
            }},
            "spec": {"taints": []}
        });
        let result = validate_node(
            "worker-1",
            Operation::Update,
            "worker-1",
            Some(&new),
            Some(&old),
        );
        assert!(
            matches!(result, Err(Error::Forbidden(message)) if message.contains("node-restriction.kubernetes.io/pwned"))
        );
    }

    #[test]
    fn node_identity_may_update_non_restricted_node_fields() {
        let old = json!({
            "metadata": {"name": "worker-1", "labels": {"kubernetes.io/hostname": "worker-1"}},
            "spec": {"taints": []}
        });
        let new = json!({
            "metadata": {"name": "worker-1", "labels": {"kubernetes.io/hostname": "worker-1"}},
            "spec": {"taints": [], "unschedulable": true}
        });
        assert!(validate_node(
            "worker-1",
            Operation::Update,
            "worker-1",
            Some(&new),
            Some(&old)
        )
        .is_ok());
    }

    #[test]
    fn mirror_pod_requires_its_node_owner_and_no_api_references() {
        let pod = json!({
            "metadata": {
                "annotations": {MIRROR_POD_ANNOTATION: "hash"},
                "ownerReferences": [{
                    "apiVersion": "v1", "kind": "Node", "name": "worker-1",
                    "uid": "node-uid", "controller": true
                }]
            },
            "spec": {"nodeName": "worker-1", "containers": [{"name": "app"}]}
        });
        assert!(!references_api_objects(&pod));

        let mut with_secret = pod;
        with_secret["spec"]["volumes"] = json!([{"name": "secret", "secret": {"secretName": "x"}}]);
        assert!(references_api_objects(&with_secret));
    }

    #[test]
    fn only_a_node_identity_is_restricted() {
        let ordinary = Identity {
            name: "alice".to_string(),
            groups: Vec::new(),
            uid: None,
            credential_id: (String::new(), Vec::new()),
        };
        assert!(node_name(Some(&node_identity())).is_some());
        assert!(node_name(Some(&ordinary)).is_none());
    }
}
