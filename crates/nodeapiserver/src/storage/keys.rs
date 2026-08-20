//! The `/registry/...` etcd key layout — the *unconfigured default* half
//! of it, verified against real upstream source rather than assumed.
//!
//! # What's verified, and what's deliberately deferred
//!
//! Confirmed directly against
//! `staging/src/k8s.io/apiserver/pkg/server/storage/storage_factory.go`
//! (`release-1.34`), `DefaultStorageFactory.ResourcePrefix`:
//!
//! ```text
//! etcdResourcePrefix := s.DefaultResourcePrefixes[chosenStorageResource]
//! // ... overrides checked here ...
//! if len(etcdResourcePrefix) == 0 {
//!     etcdResourcePrefix = strings.ToLower(chosenStorageResource.Resource)
//! }
//! ```
//!
//! So the *default* (nothing overridden) prefix for a resource is simply
//! its own lowercased plural name — **no group suffix**, contrary to the
//! shorthand `docs/APISERVER_PLAN.md` uses. The group-suffixed form
//! (`replicasets.apps`, etc.) exists only in `DefaultResourcePrefixes`,
//! `pkg/controlplane/instance.go`'s hand-maintained override table — real,
//! separate work to vendor and parse, not something to approximate from
//! memory. Every resource this module handles correctly *today* is one
//! with no override; a resource that has one will get the wrong key until
//! that table lands, which is exactly why `resource_prefix()` below is
//! `pub(crate)` rather than exposed as a finished public API yet.
//!
//! `NamespaceKeyRootFunc`/`NamespaceKeyFunc`
//! (`staging/src/k8s.io/apiserver/pkg/registry/generic/registry/store.go`)
//! confirm the rest of the shape: `<prefix>/<namespace>/<name>` for a
//! namespaced resource, `<prefix>/<name>` for a cluster-scoped one. The
//! leading `/registry` root itself (`DefaultEtcdPathPrefix`) is
//! long-stable, documented Kubernetes behavior (visible in any real
//! cluster's etcd dump), not something this module needed to re-derive.

/// The default etcd key root every resource's own prefix is joined under.
/// Configurable upstream (`--etcd-prefix`); not configurable here yet —
/// added when something needs it to be.
const REGISTRY_ROOT: &str = "/registry";

/// The *unconfigured-default* resource prefix: the resource's own plural
/// name, lowercased, with no group suffix. See this module's own doc
/// comment for why a real override table is still missing.
pub(crate) fn resource_prefix(resource: &str) -> String {
    resource.to_ascii_lowercase()
}

/// The etcd key for one object. `namespace: None` for a cluster-scoped
/// resource.
pub fn object_key(resource: &str, namespace: Option<&str>, name: &str) -> String {
    match namespace {
        Some(ns) if !ns.is_empty() => format!("{REGISTRY_ROOT}/{}/{ns}/{name}", resource_prefix(resource)),
        _ => format!("{REGISTRY_ROOT}/{}/{name}", resource_prefix(resource)),
    }
}

/// The etcd key *prefix* for a LIST/WATCH over every object of a resource
/// — the whole resource, or scoped to one namespace.
pub fn list_prefix(resource: &str, namespace: Option<&str>) -> String {
    match namespace {
        Some(ns) if !ns.is_empty() => format!("{REGISTRY_ROOT}/{}/{ns}/", resource_prefix(resource)),
        _ => format!("{REGISTRY_ROOT}/{}/", resource_prefix(resource)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_namespaced_object_key_matches_upstreams_documented_shape() {
        // The textbook example from every real cluster's etcd dump.
        assert_eq!(object_key("pods", Some("default"), "my-pod"), "/registry/pods/default/my-pod");
    }

    #[test]
    fn a_cluster_scoped_object_key_has_no_namespace_segment() {
        assert_eq!(object_key("namespaces", None, "kube-system"), "/registry/namespaces/kube-system");
    }

    #[test]
    fn an_empty_namespace_is_treated_as_cluster_scoped() {
        assert_eq!(object_key("clusterroles", Some(""), "admin"), "/registry/clusterroles/admin");
    }

    #[test]
    fn the_resource_prefix_is_lowercased_with_no_group_suffix_by_default() {
        assert_eq!(resource_prefix("Pods"), "pods");
    }

    #[test]
    fn list_prefix_ends_in_a_trailing_slash_so_it_only_matches_this_resource() {
        // Without the trailing slash, a range prefix of "/registry/pod"
        // would also match a hypothetical "/registry/poddisruptionbudgets"
        // key — the trailing separator is what makes it an exact directory
        // prefix instead of a string prefix.
        let prefix = list_prefix("pods", Some("default"));
        assert!(prefix.ends_with('/'));
        assert_eq!(prefix, "/registry/pods/default/");
    }
}
