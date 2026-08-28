//! The built-in Node authorizer's identity and safe request rules.
//!
//! Kubernetes identifies kubelets by the `system:node:<node-name>` username.
//! The upstream Node authorizer is deliberately a deny-capable authorizer:
//! once a request is recognized as coming from a node, an unsupported or
//! unrelated request must not fall through to a broad RBAC role.  The rules
//! here cover the request shapes that can be decided without reading the
//! pod/secret graph.  Graph-dependent object access stays denied until its
//! storage-backed informer is implemented.

use crate::authn::x509::Identity;
use crate::authz::rbac::RequestAttributes;

const NODE_PREFIX: &str = "system:node:";
const NODES_RESOURCE: &str = "nodes";
const PODS_RESOURCE: &str = "pods";
const LEASES_GROUP: &str = "coordination.k8s.io";
const LEASES_RESOURCE: &str = "leases";
const NODE_LEASE_NAMESPACE: &str = "kube-node-lease";

/// The three outcomes an authorizer can return.  `NoOpinion` is reserved for
/// identities that are not nodes; a recognized node identity is either
/// explicitly allowed or denied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Allow,
    Deny,
    NoOpinion,
}

/// Return the node name carried by a Kubernetes node identity.
pub fn node_name(identity: Option<&Identity>) -> Option<&str> {
    let name = identity?.name.strip_prefix(NODE_PREFIX)?;
    (!name.is_empty()).then_some(name)
}

/// Apply the part of the Node authorizer that is decidable from request
/// attributes alone.  Node identities never fall through to RBAC: callers
/// must honor `Deny` before evaluating any RBAC rules.
pub fn authorize(identity: Option<&Identity>, attrs: &RequestAttributes<'_>, api_version: &str, field_selector: &str, namespace: Option<&str>) -> Decision {
    let Some(node) = node_name(identity) else {
        return Decision::NoOpinion;
    };

    if !attrs.is_resource_request {
        return Decision::Deny;
    }

    if attrs.api_group.is_empty() && api_version == "v1" && attrs.resource == NODES_RESOURCE {
        if attrs.name != node {
            return Decision::Deny;
        }
        return match (attrs.verb, attrs.subresource) {
            ("get", "") | ("get", "status") => Decision::Allow,
            ("update" | "patch", "status") => Decision::Allow,
            _ => Decision::Deny,
        };
    }

    // Kubelet's pod informer is scoped with this exact selector.  Do not
    // allow an unscoped list/watch: it would expose every node's workload.
    if attrs.api_group.is_empty() && api_version == "v1" && attrs.resource == PODS_RESOURCE && attrs.subresource.is_empty() && matches!(attrs.verb, "list" | "watch") {
        return if field_selector == format!("spec.nodeName={node}") { Decision::Allow } else { Decision::Deny };
    }

    // Kubelet heartbeats use the node's own Lease in kube-node-lease.  A
    // create request has no name in RequestInfo, so it is permitted only in
    // this dedicated namespace; updates/patches/gets must name this node.
    if attrs.api_group == LEASES_GROUP && api_version == "v1" && attrs.resource == LEASES_RESOURCE && attrs.subresource.is_empty() && namespace == Some(NODE_LEASE_NAMESPACE) {
        if attrs.verb == "create" && attrs.name.is_empty() {
            return Decision::Allow;
        }
        if attrs.name == node && matches!(attrs.verb, "get" | "update" | "patch" | "delete") {
            return Decision::Allow;
        }
        return Decision::Deny;
    }

    // Access to a named Pod, Secret, ConfigMap, PVC, PV, Event, and CSI
    // object depends on the live pod/object graph.  Until that graph is
    // informer-backed, fail closed instead of allowing a broad RBAC rule to
    // bypass the Node authorizer.
    Decision::Deny
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity(name: &str) -> Identity {
        Identity { name: name.to_string(), groups: vec!["system:nodes".to_string()], credential_id: (String::new(), Vec::new()) }
    }

    fn attrs<'a>(verb: &'a str, group: &'a str, resource: &'a str, subresource: &'a str, name: &'a str) -> RequestAttributes<'a> {
        RequestAttributes { is_resource_request: true, verb, api_group: group, resource, subresource, name, path: "" }
    }

    #[test]
    fn non_node_identity_is_not_opined_on() {
        let request = attrs("get", "", "pods", "", "pod");
        assert_eq!(authorize(Some(&identity("alice")), &request, "v1", "", Some("default")), Decision::NoOpinion);
    }

    #[test]
    fn node_can_read_its_own_node_but_not_another_node() {
        let node = identity("system:node:worker-a");
        let own = attrs("get", "", "nodes", "", "worker-a");
        let other = attrs("get", "", "nodes", "", "worker-b");
        assert_eq!(authorize(Some(&node), &own, "v1", "", None), Decision::Allow);
        assert_eq!(authorize(Some(&node), &other, "v1", "", None), Decision::Deny);
    }

    #[test]
    fn node_can_list_only_pods_selected_for_it() {
        let node = identity("system:node:worker-a");
        let request = attrs("list", "", "pods", "", "");
        assert_eq!(authorize(Some(&node), &request, "v1", "spec.nodeName=worker-a", None), Decision::Allow);
        assert_eq!(authorize(Some(&node), &request, "v1", "", None), Decision::Deny);
        assert_eq!(authorize(Some(&node), &request, "v1", "spec.nodeName=worker-b", None), Decision::Deny);
    }

    #[test]
    fn graph_dependent_pod_access_is_not_accidentally_allowed() {
        let node = identity("system:node:worker-a");
        let request = attrs("get", "", "pods", "", "pod");
        assert_eq!(authorize(Some(&node), &request, "v1", "", Some("default")), Decision::Deny);
    }
}
