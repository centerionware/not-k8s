//! Discovery documents: `/api` (`APIVersions`), `/apis` (`APIGroupList`),
//! `/apis/{group}` (`APIGroup`) — built entirely from Group A's
//! `codegen::openapi_meta::DISCOVERY_GVKS` table, so nothing here
//! hand-maintains a list of which groups/versions this build actually
//! serves.
//!
//! Real shapes (`APIVersions`/`APIGroupList`/`APIGroup`/
//! `GroupVersionForDiscovery`) confirmed directly against the vendored
//! OpenAPI v3 specs, not assumed from memory of the stable v1 meta types.
//!
//! Per-version resource listing (`/api/v1`, `/apis/{group}/{version}` ->
//! `APIResourceList`) is `api_resource_list()`, built from
//! `codegen::api_resources_by_group_version()` — itself from a new Group A
//! table (`build/discovery_parse.rs`) that parses the OpenAPI spec's own
//! `paths` section (each verb block's `x-kubernetes-action` +
//! `x-kubernetes-group-version-kind`), closing the gap this module's doc
//! comment used to name here. Deliberately still missing from each
//! `APIResource` entry: `singularName` (not present anywhere in the
//! vendored spec — real kube-apiserver derives it from Go type reflection,
//! which this crate has no equivalent of), `shortNames`, `categories` (same
//! reason), and subresources (`pods/status`, `pods/log`, ... — a named,
//! separate skip in the parser itself, not a completeness claim by this
//! module).
//!
//! `serverAddressByClientCIDRs` is left empty in every document here —
//! real kube-apiserver populates it from the request's own observed
//! client address once it has one; there is no HTTP request in scope yet
//! for these pure builder functions to read that from (Group E's handler
//! chain, once it exists, is what would thread a real value through).

use crate::codegen;
use serde_json::{json, Value};
use std::collections::BTreeMap;

/// `/api` — the legacy, groupless core group's own version list. This
/// build always serves exactly `v1` for the core group (verified: every
/// `DISCOVERY_GVKS` entry with `group == ""` has `version == "v1"`, since
/// v1.34 vendors nothing else for the core group).
pub fn api_versions() -> Value {
    let mut versions: Vec<&str> = codegen::openapi_meta::DISCOVERY_GVKS.iter().filter(|g| g.group.is_empty()).map(|g| g.version).collect();
    versions.sort_unstable();
    versions.dedup();
    json!({
        "kind": "APIVersions",
        "versions": versions,
        "serverAddressByClientCIDRs": [],
    })
}

/// `/apis` — every non-core group this build serves, each with its
/// versions (kube-aware-sorted, most preferred first) and its preferred
/// version.
pub fn api_group_list() -> Value {
    let groups = group_version_map();
    let list: Vec<Value> = groups.keys().map(|group| api_group_value(group, &groups[group])).collect();
    json!({
        "kind": "APIGroupList",
        "apiVersion": "v1",
        "groups": list,
    })
}

/// `/apis/{group}` — `None` if this build serves no such group.
pub fn api_group(group: &str) -> Option<Value> {
    let groups = group_version_map();
    let versions = groups.get(group)?;
    Some(api_group_value(group, versions))
}

/// `group -> [version, ...]`, deduplicated, from the discovery table. The
/// core group (`""`) is deliberately excluded — it has no `/apis/{group}`
/// document of its own, only `/api`.
fn group_version_map() -> BTreeMap<&'static str, Vec<&'static str>> {
    let mut groups: BTreeMap<&'static str, Vec<&'static str>> = BTreeMap::new();
    for g in codegen::openapi_meta::DISCOVERY_GVKS {
        if g.group.is_empty() {
            continue;
        }
        let versions = groups.entry(g.group).or_default();
        if !versions.contains(&g.version) {
            versions.push(g.version);
        }
    }
    for versions in groups.values_mut() {
        sort_versions_most_preferred_first(versions);
    }
    groups
}

fn api_group_value(group: &str, versions: &[&str]) -> Value {
    let group_version = |v: &str| format!("{group}/{v}");
    let version_values: Vec<Value> = versions.iter().map(|v| json!({"groupVersion": group_version(v), "version": v})).collect();
    let preferred = versions.first().map(|v| json!({"groupVersion": group_version(v), "version": v})).unwrap_or(json!({}));
    json!({
        "kind": "APIGroup",
        "apiVersion": "v1",
        "name": group,
        "versions": version_values,
        "preferredVersion": preferred,
        "serverAddressByClientCIDRs": [],
    })
}

/// `/api/{version}` or `/apis/{group}/{version}` — every resource this
/// build serves for that exact group+version, `None` if this build serves
/// no such group+version at all (as opposed to serving it with zero
/// resources, which shouldn't happen against the real vendored data but
/// would render as an empty `resources: []` rather than `None` — a
/// genuinely served group+version always has at least one resource).
pub fn api_resource_list(group: &str, version: &str) -> Option<Value> {
    let resources = codegen::api_resources_by_group_version().get(&(group, version))?;
    let group_version = if group.is_empty() { version.to_string() } else { format!("{group}/{version}") };
    let mut sorted: Vec<&codegen::api_resources::ApiResource> = resources.iter().copied().collect();
    sorted.sort_by_key(|r| r.resource);
    let list: Vec<Value> = sorted
        .iter()
        .map(|r| {
            let mut verbs: Vec<&str> = r.verbs.to_vec();
            verbs.sort_unstable();
            json!({
                "name": r.resource,
                // Real kube-apiserver's own RESTMapper default when a type
                // doesn't declare an explicit singular form — this crate
                // has no per-type override table (see this module's own
                // doc comment), so every entry uses the default.
                "singularName": r.kind.to_lowercase(),
                "namespaced": r.namespaced,
                "kind": r.kind,
                "verbs": verbs,
            })
        })
        .collect();
    Some(json!({
        "kind": "APIResourceList",
        "apiVersion": "v1",
        "groupVersion": group_version,
        "resources": list,
    }))
}

/// Sorts `versions` most-preferred-first, per
/// [`super::version_compare::compare_kube_aware_versions`].
fn sort_versions_most_preferred_first(versions: &mut [&'static str]) {
    versions.sort_unstable_by(|a, b| super::version_compare::compare_kube_aware_versions(b, a));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn api_versions_serves_core_v1_only() {
        let v = api_versions();
        assert_eq!(v["kind"], "APIVersions");
        assert_eq!(v["versions"], json!(["v1"]));
    }

    #[test]
    fn apps_group_is_present_with_v1_preferred() {
        let group = api_group("apps").expect("apps group should be in the vendored discovery table");
        assert_eq!(group["kind"], "APIGroup");
        assert_eq!(group["name"], "apps");
        assert_eq!(group["preferredVersion"]["version"], "v1");
        assert_eq!(group["preferredVersion"]["groupVersion"], "apps/v1");
    }

    /// `resource.k8s.io` genuinely has multiple versions in the vendored
    /// v1.34 set (v1, v1beta1, v1beta2 at least — finding 10) — a real
    /// multi-version group to prove the preferred-version selection
    /// against, not a synthetic one.
    #[test]
    fn a_real_multi_version_group_picks_the_ga_version_as_preferred() {
        let group = api_group("resource.k8s.io").expect("resource.k8s.io should be in the vendored discovery table");
        let versions: Vec<&str> = group["versions"].as_array().unwrap().iter().map(|v| v["version"].as_str().unwrap()).collect();
        assert!(versions.len() > 1, "expected a genuinely multi-version group, got {versions:?}");
        assert_eq!(group["preferredVersion"]["version"], "v1", "the GA version must be preferred over any beta");
    }

    #[test]
    fn an_unknown_group_is_none() {
        assert!(api_group("totally.made.up.group").is_none());
    }

    #[test]
    fn api_resource_list_serves_core_v1_pods_with_real_verbs() {
        let list = api_resource_list("", "v1").expect("core/v1 should be served");
        assert_eq!(list["kind"], "APIResourceList");
        assert_eq!(list["groupVersion"], "v1");
        let resources = list["resources"].as_array().unwrap();
        let pods = resources.iter().find(|r| r["name"] == "pods").expect("pods should be listed");
        assert_eq!(pods["namespaced"], true);
        assert_eq!(pods["kind"], "Pod");
        assert_eq!(pods["singularName"], "pod");
        let verbs: Vec<&str> = pods["verbs"].as_array().unwrap().iter().map(|v| v.as_str().unwrap()).collect();
        assert!(verbs.contains(&"watch"));
    }

    #[test]
    fn api_resource_list_for_a_non_core_group_uses_group_slash_version() {
        let list = api_resource_list("apps", "v1").expect("apps/v1 should be served");
        assert_eq!(list["groupVersion"], "apps/v1");
        let resources = list["resources"].as_array().unwrap();
        assert!(resources.iter().any(|r| r["name"] == "deployments"));
    }

    #[test]
    fn api_resource_list_for_an_unserved_group_version_is_none() {
        assert!(api_resource_list("apps", "v999").is_none());
    }

    #[test]
    fn api_group_list_contains_every_non_core_group_and_never_the_core_group_itself() {
        let list = api_group_list();
        let names: Vec<&str> = list["groups"].as_array().unwrap().iter().map(|g| g["name"].as_str().unwrap()).collect();
        assert!(names.contains(&"apps"));
        assert!(!names.contains(&""), "the core group belongs to /api, not /apis");
    }
}
