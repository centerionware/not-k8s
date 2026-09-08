//! The `/registry/...` etcd key layout — verified against real upstream
//! source rather than assumed, including the resource-prefix override
//! table this module's own history once deferred (see `git log` for the
//! round where that was still an open gap; it's closed now).
//!
//! # What's verified
//!
//! `DefaultStorageFactory.ResourcePrefix`
//! (`staging/src/k8s.io/apiserver/pkg/server/storage/storage_factory.go`,
//! `release-1.34`):
//!
//! ```text
//! etcdResourcePrefix := s.DefaultResourcePrefixes[chosenStorageResource]
//! // ... per-group-resource and per-exact-resource overrides checked here ...
//! if len(etcdResourcePrefix) == 0 {
//!     etcdResourcePrefix = strings.ToLower(chosenStorageResource.Resource)
//! }
//! ```
//!
//! So the prefix is the resource's own lowercased plural name **unless**
//! `(group, resource)` appears in the override table, in which case that
//! wins. The override table itself is `SpecialDefaultResourcePrefixes`,
//! `pkg/kubeapiserver/default_storage_factory_builder.go` (moved there from
//! `pkg/controlplane/instance.go` at some point between this plan's
//! original research and `release-1.34` — found via GitHub code search
//! rather than assumed still at the old path). It is genuinely small:
//!
//! ```text
//! {Group: "",                   Resource: "replicationcontrollers"}: "controllers",
//! {Group: "",                   Resource: "endpoints"}:              "services/endpoints",
//! {Group: "",                   Resource: "nodes"}:                  "minions",
//! {Group: "",                   Resource: "services"}:               "services/specs",
//! {Group: "extensions",         Resource: "ingresses"}:              "ingress",
//! {Group: "networking.k8s.io",  Resource: "ingresses"}:              "ingress",
//! ```
//!
//! `NamespaceKeyRootFunc`/`NamespaceKeyFunc`
//! (`staging/src/k8s.io/apiserver/pkg/registry/generic/registry/store.go`)
//! confirm the rest of the shape: `<prefix>/<namespace>/<name>` for a
//! namespaced resource, `<prefix>/<name>` for a cluster-scoped one. The
//! leading `/registry` root itself (`DefaultEtcdPathPrefix`) is
//! long-stable, documented Kubernetes behavior (visible in any real
//! cluster's etcd dump), not something this module needed to re-derive.
//!
//! # What's still not modeled
//!
//! `EtcdServersOverrides` (per-resource storage backend/location overrides)
//! and multi-version storage encoding (`ResourceEncodingOverrides`) are
//! separate concerns from key *layout* and belong to Group F/the storage
//! backend selection, not this module.

/// `(group, resource) -> prefix override`. `""` is the core group, matching
/// every other group string in this codebase (never `"core"`).
const OVERRIDES: &[((&str, &str), &str)] = &[
    (("", "replicationcontrollers"), "controllers"),
    (("", "endpoints"), "services/endpoints"),
    (("", "nodes"), "minions"),
    (("", "services"), "services/specs"),
    (("extensions", "ingresses"), "ingress"),
    (("networking.k8s.io", "ingresses"), "ingress"),
];

/// The default etcd key root every resource's own prefix is joined under.
/// Configurable upstream (`--etcd-prefix`); not configurable here yet —
/// added when something needs it to be.
const REGISTRY_ROOT: &str = "/registry";

/// The resource prefix: the override table's value if `(group, resource)`
/// is in it, else the resource's own plural name lowercased. `group` is
/// `""` for the core group, matching how every GVK is spelled elsewhere in
/// this codebase.
pub fn resource_prefix(group: &str, resource: &str) -> String {
    let lower = resource.to_ascii_lowercase();
    OVERRIDES.iter().find(|((g, r), _)| *g == group && *r == lower).map(|(_, prefix)| prefix.to_string()).unwrap_or(lower)
}

/// The etcd key for one object. `namespace: None` for a cluster-scoped
/// resource.
pub fn object_key(group: &str, resource: &str, namespace: Option<&str>, name: &str) -> String {
    match namespace {
        Some(ns) if !ns.is_empty() => format!("{REGISTRY_ROOT}/{}/{ns}/{name}", resource_prefix(group, resource)),
        _ => format!("{REGISTRY_ROOT}/{}/{name}", resource_prefix(group, resource)),
    }
}

/// The etcd key *prefix* for a LIST/WATCH over every object of a resource
/// — the whole resource, or scoped to one namespace.
pub fn list_prefix(group: &str, resource: &str, namespace: Option<&str>) -> String {
    match namespace {
        Some(ns) if !ns.is_empty() => format!("{REGISTRY_ROOT}/{}/{ns}/", resource_prefix(group, resource)),
        _ => format!("{REGISTRY_ROOT}/{}/", resource_prefix(group, resource)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_namespaced_object_key_matches_upstreams_documented_shape() {
        // The textbook example from every real cluster's etcd dump.
        assert_eq!(object_key("", "pods", Some("default"), "my-pod"), "/registry/pods/default/my-pod");
    }

    #[test]
    fn a_cluster_scoped_object_key_has_no_namespace_segment() {
        assert_eq!(object_key("", "namespaces", None, "kube-system"), "/registry/namespaces/kube-system");
    }

    #[test]
    fn an_empty_namespace_is_treated_as_cluster_scoped() {
        assert_eq!(object_key("rbac.authorization.k8s.io", "clusterroles", Some(""), "admin"), "/registry/clusterroles/admin");
    }

    #[test]
    fn the_default_resource_prefix_is_lowercased_with_no_group_suffix() {
        assert_eq!(resource_prefix("apps", "Deployments"), "deployments");
    }

    #[test]
    fn every_documented_override_is_applied_exactly() {
        assert_eq!(resource_prefix("", "replicationcontrollers"), "controllers");
        assert_eq!(resource_prefix("", "endpoints"), "services/endpoints");
        assert_eq!(resource_prefix("", "nodes"), "minions");
        assert_eq!(resource_prefix("", "services"), "services/specs");
        assert_eq!(resource_prefix("extensions", "ingresses"), "ingress");
        assert_eq!(resource_prefix("networking.k8s.io", "ingresses"), "ingress");
    }

    /// The override table is keyed by `(group, resource)`, not `resource`
    /// alone — the same resource name in a *different* group must not pick
    /// up an override that only applies to a specific group.
    #[test]
    fn overrides_are_scoped_to_their_exact_group_not_just_the_resource_name() {
        assert_eq!(resource_prefix("apps", "ingresses"), "ingresses", "apps/ingresses isn't a real resource, but the point stands: no group means no override");
        assert_eq!(resource_prefix("", "ingresses"), "ingresses", "core-group ingresses (not a real resource either) must not inherit extensions'/networking's override");
    }

    #[test]
    fn an_overridden_prefix_produces_the_real_multi_segment_etcd_key() {
        // /registry/services/specs/..., not /registry/services/... — the
        // whole reason this table exists at all: "services" the resource
        // and "services/specs" the etcd directory are different strings.
        assert_eq!(object_key("", "services", Some("default"), "my-svc"), "/registry/services/specs/default/my-svc");
        assert_eq!(object_key("", "endpoints", Some("default"), "my-svc"), "/registry/services/endpoints/default/my-svc");
        assert_eq!(object_key("", "nodes", None, "worker-1"), "/registry/minions/worker-1");
    }

    #[test]
    fn list_prefix_ends_in_a_trailing_slash_so_it_only_matches_this_resource() {
        // Without the trailing slash, a range prefix of "/registry/pod"
        // would also match a hypothetical "/registry/poddisruptionbudgets"
        // key — the trailing separator is what makes it an exact directory
        // prefix instead of a string prefix.
        let prefix = list_prefix("", "pods", Some("default"));
        assert!(prefix.ends_with('/'));
        assert_eq!(prefix, "/registry/pods/default/");
    }
}
