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
//! **Scoped to group-level discovery only.** The per-version resource
//! listing (`/api/v1`, `/apis/{group}/{version}` -> `APIResourceList`,
//! naming every resource's plural name, namespaced flag, and supported
//! verbs) needs data this crate hasn't vendored yet: `DISCOVERY_GVKS` only
//! carries `(schema, group, version, kind)`, not the resource's REST path
//! shape. Group A's codegen would need to also parse the OpenAPI spec's
//! `paths` section (or Group F's registry would need to supply it
//! directly) — real, separate work, not implied by this module's own
//! completeness.
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
    fn api_group_list_contains_every_non_core_group_and_never_the_core_group_itself() {
        let list = api_group_list();
        let names: Vec<&str> = list["groups"].as_array().unwrap().iter().map(|g| g["name"].as_str().unwrap()).collect();
        assert!(names.contains(&"apps"));
        assert!(!names.contains(&""), "the core group belongs to /api, not /apis");
    }
}
