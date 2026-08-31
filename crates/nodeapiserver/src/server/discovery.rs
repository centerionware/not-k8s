//! Discovery documents: `/api` (`APIVersions`), `/apis` (`APIGroupList`),
//! `/apis/{group}` (`APIGroup`) — built entirely from Group A's generated
//! resource table, so non-resource schemas such as `DeleteOptions` and
//! `WatchEvent` cannot falsely advertise an API group or version.
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
//! comment used to name here. `singularName` is derived from the kind and
//! the standard built-in `shortNames` are supplied by the explicit
//! compatibility table below; neither is present in the vendored spec.
//! Subresources (`pods/status`, `pods/log`, ...) are emitted as their own
//! `APIResource` entries from the generated table, including connect verbs
//! where the OpenAPI paths advertise them.
//!
//! `serverAddressByClientCIDRs` is left empty in every document here —
//! real kube-apiserver populates it from the request's own observed
//! client address once it has one; there is no HTTP request in scope yet
//! for these pure builder functions to read that from (Group E's handler
//! chain, once it exists, is what would thread a real value through).
//!
//! Aggregated discovery v2 (`apidiscovery.k8s.io/v2`'s
//! `APIGroupDiscoveryList`) is `api_group_discovery_list()`/
//! `api_v1_group_discovery_list()`, wired into `listener`'s `/api`/`/apis`
//! routing via `codec::negotiation` — a client requesting
//! `as=APIGroupDiscoveryList;v=v2;g=apidiscovery.k8s.io` gets this shape
//! instead of the legacy one.

use crate::apiextensions::registry::DiscoverableResource;
use crate::codegen;
use serde_json::{json, Value};
use std::collections::BTreeMap;

/// Every real, generic verb `server::rest` serves for a CRD-defined
/// resource as of Group K's own current scope (`create`/`get`/`list`/
/// `update`/`patch`/`delete`/`deletecollection`/`watch` — see
/// `apiextensions::mod`'s own doc comment for the exact landed set) —
/// real upstream's own `crdHandler` installs identical generic storage
/// for every CRD (`pkg/apiserver/customresource_handler.go`), so this
/// build's own uniform verb list for every CRD-backed resource matches
/// that same "no per-Kind customization" posture, not an approximation.
const CRD_VERBS: &[&str] = &["create", "delete", "deletecollection", "get", "list", "patch", "update", "watch"];

/// Kubernetes discovery short names are API conventions rather than OpenAPI
/// metadata. Keep the compatibility aliases here so clients such as kubectl
/// can resolve familiar names (`pvc`, `deploy`, `svc`, ...) against the
/// built-in resources just as they do against kube-apiserver.
fn short_names_for(group: &str, resource: &str) -> &'static [&'static str] {
    match (group, resource) {
        ("", "componentstatuses") => &["cs"],
        ("", "configmaps") => &["cm"],
        ("", "endpoints") => &["ep"],
        ("", "events") => &["ev"],
        ("", "limitranges") => &["limits"],
        ("", "namespaces") => &["ns"],
        ("", "nodes") => &["no"],
        ("", "persistentvolumeclaims") => &["pvc"],
        ("", "persistentvolumes") => &["pv"],
        ("", "pods") => &["po"],
        ("", "replicationcontrollers") => &["rc"],
        ("", "resourcequotas") => &["quota"],
        ("", "secrets") => &["secret"],
        ("", "serviceaccounts") => &["sa"],
        ("", "services") => &["svc"],
        ("apps", "daemonsets") => &["ds"],
        ("apps", "deployments") => &["deploy"],
        ("apps", "replicasets") => &["rs"],
        ("apps", "statefulsets") => &["sts"],
        ("batch", "cronjobs") => &["cj"],
        ("batch", "jobs") => &["job"],
        ("certificates.k8s.io", "certificatesigningrequests") => &["csr"],
        ("networking.k8s.io", "ingresses") => &["ing"],
        ("networking.k8s.io", "networkpolicies") => &["netpol"],
        ("node.k8s.io", "runtimeclasses") => &["rc"],
        ("policy", "poddisruptionbudgets") => &["pdb"],
        ("scheduling.k8s.io", "priorityclasses") => &["pc"],
        ("storage.k8s.io", "storageclasses") => &["sc"],
        _ => &[],
    }
}

/// Kubernetes discovery categories are API conventions rather than OpenAPI
/// metadata. The `all` category lets clients such as kubectl expand
/// `kubectl get/describe all` into the standard workload resources.
fn categories_for(group: &str, resource: &str) -> &'static [&'static str] {
    match (group, resource) {
        ("", "pods" | "replicationcontrollers" | "services")
        | ("apps", "daemonsets" | "deployments" | "replicasets" | "statefulsets")
        | ("autoscaling", "horizontalpodautoscalers")
        | ("batch", "cronjobs" | "jobs") => &["all"],
        _ => &[],
    }
}

/// `/api` — the legacy, groupless core group's own version list. This
/// build always serves exactly `v1` for the core group (verified: every
/// generated resource entry with `group == ""` has `version == "v1"`, since
/// v1.34 vendors nothing else for the core group).
pub fn api_versions() -> Value {
    let mut versions: Vec<&str> = codegen::api_resources_by_group_version()
        .keys()
        .filter_map(|(group, version)| group.is_empty().then_some(*version))
        .collect();
    versions.sort_unstable();
    versions.dedup();
    json!({
        "apiVersion": "v1",
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

/// `group -> [version, ...]`, deduplicated, from the generated resource
/// table. The core group (`""`) is deliberately excluded — it has no
/// `/apis/{group}` document of its own, only `/api`.
fn group_version_map() -> BTreeMap<&'static str, Vec<&'static str>> {
    let mut groups: BTreeMap<&'static str, Vec<&'static str>> = BTreeMap::new();
    for &(group, version) in codegen::api_resources_by_group_version().keys() {
        if group.is_empty() {
            continue;
        }
        let versions = groups.entry(group).or_default();
        if !versions.contains(&version) {
            versions.push(version);
        }
    }
    for versions in groups.values_mut() {
        sort_versions_most_preferred_first(versions);
    }
    groups
}

/// The dynamic counterpart to [`group_version_map`]: the static table,
/// folded together with whatever `(group, version)` pairs `crds`
/// provides — `server::listener`'s own discovery routing fetches `crds`
/// live (`rest::list_all_crds`, one `LIST` of `customresourcedefinitions`,
/// only for an `/apis`-prefixed request) and passes them in; this module
/// stays free of any I/O itself, same posture the rest of it already has.
/// Owned `String`s throughout (unlike `group_version_map`'s own
/// `&'static str`), since a CRD's own group/version strings are never
/// `'static` — this build's own well-founded reason every `*_with_crds`
/// function below exists as a genuinely separate function from its
/// static-only counterpart rather than a shared generic.
/// `aggregated` is Group L's own third merge input — `(group, version)`
/// pairs from stored, non-local `APIService`s whose own live pre-flight
/// check (`aggregator::availability::preflight_check`, run fresh by the
/// caller — `server::listener`, same "no I/O in this module" posture
/// every other discovery builder here already holds itself to) currently
/// passes. **Deliberately only ever merged into the two *group-level*
/// documents** (`api_group_list_with_crds`/`api_group_with_crds` below)
/// — never into [`api_resource_list_with_crds`]: unlike a CRD (which
/// declares its own resource's `kind`/`namespaced`/schema up front, so
/// this build genuinely knows what it serves), an aggregated backend's
/// own resource list is only known to *it* — real upstream's own
/// `/apis/{group}/{version}` for an aggregated API is itself a live
/// proxied fetch to the backend, not a locally-synthesizable document,
/// so this module (pure, no I/O) has no business answering it at all.
/// That live dial *is* wired in, just one layer up: `server::listener::
/// handle` catches a plain `GET /apis/{group}/{version}` for one of
/// `aggregated`'s own pairs (this exact function's own `NotFound`
/// outcome from `api_resource_list_with_crds`, which never had a local
/// answer for it) and proxies it through `aggregate_proxy` instead of
/// ever calling into this module for that path. So both `kubectl
/// api-versions` and `kubectl api-resources` genuinely work against an
/// aggregated group now.
fn merged_group_version_map(crds: &[DiscoverableResource], aggregated: &[(String, String)]) -> BTreeMap<String, Vec<String>> {
    let mut groups: BTreeMap<String, Vec<String>> = group_version_map().into_iter().map(|(g, vs)| (g.to_string(), vs.into_iter().map(str::to_string).collect())).collect();
    for r in crds {
        let versions = groups.entry(r.group.clone()).or_default();
        if !versions.contains(&r.version) {
            versions.push(r.version.clone());
        }
    }
    for (group, version) in aggregated {
        let versions = groups.entry(group.clone()).or_default();
        if !versions.contains(version) {
            versions.push(version.clone());
        }
    }
    for versions in groups.values_mut() {
        versions.sort_unstable_by(|a, b| super::version_compare::compare_kube_aware_versions(b, a));
    }
    groups
}

/// `/apis`, merging in every group a served, `Established` CRD provides,
/// and every group a currently-available aggregated `APIService`
/// provides, on top of the static table — see
/// [`merged_group_version_map`]'s own doc comment for where `crds`/
/// `aggregated` come from.
pub fn api_group_list_with_crds(crds: &[DiscoverableResource], aggregated: &[(String, String)]) -> Value {
    let groups = merged_group_version_map(crds, aggregated);
    let list: Vec<Value> = groups.keys().map(|group| api_group_value_owned(group, &groups[group])).collect();
    json!({
        "kind": "APIGroupList",
        "apiVersion": "v1",
        "groups": list,
    })
}

/// `/apis/{group}`, merged — `None` if this build serves no such group
/// at all, neither statically nor via any CRD nor via any aggregated
/// `APIService`.
pub fn api_group_with_crds(group: &str, crds: &[DiscoverableResource], aggregated: &[(String, String)]) -> Option<Value> {
    let groups = merged_group_version_map(crds, aggregated);
    let versions = groups.get(group)?;
    Some(api_group_value_owned(group, versions))
}

/// Same shape [`api_group_value`] builds, over owned `String` versions
/// instead of `&'static str` — see [`merged_group_version_map`]'s own
/// doc comment for why that split exists.
fn api_group_value_owned(group: &str, versions: &[String]) -> Value {
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
            let mut value = json!({
                "name": r.resource,
                // Real kube-apiserver's own RESTMapper default when a type
                // doesn't declare an explicit singular form — this crate
                // has no per-type override table (see this module's own
                // doc comment), so every entry uses the default.
                "singularName": r.kind.to_lowercase(),
                "namespaced": r.namespaced,
                "kind": r.kind,
                "verbs": verbs,
            });
            if r.response_group != group || r.response_version != version {
                value["group"] = json!(r.response_group);
                value["version"] = json!(r.response_version);
            }
            let short_names = short_names_for(group, r.resource);
            if !short_names.is_empty() {
                value["shortNames"] = json!(short_names);
            }
            let categories = categories_for(group, r.resource);
            if !categories.is_empty() {
                value["categories"] = json!(categories);
            }
            value
        })
        .collect();
    Some(json!({
        "kind": "APIResourceList",
        "apiVersion": "v1",
        "groupVersion": group_version,
        "resources": list,
    }))
}

/// The dynamic counterpart to [`api_resource_list`]: `None` only when
/// this build serves the exact `(group, version)` neither statically nor
/// via any CRD in `crds` — a group+version a CRD alone provides (the
/// overwhelmingly common CRD case, since a CRD's own group is never also
/// a built-in one) still returns `Some`, just with `resources` built
/// entirely from `crds`.
pub fn api_resource_list_with_crds(group: &str, version: &str, crds: &[DiscoverableResource]) -> Option<Value> {
    let static_resources = codegen::api_resources_by_group_version().get(&(group, version));
    let dynamic: Vec<&DiscoverableResource> = crds.iter().filter(|r| r.group == group && r.version == version).collect();
    if static_resources.is_none() && dynamic.is_empty() {
        return None;
    }
    let group_version = if group.is_empty() { version.to_string() } else { format!("{group}/{version}") };
    let mut list: Vec<Value> = static_resources
        .map(|resources| {
            let mut sorted: Vec<&codegen::api_resources::ApiResource> = resources.iter().copied().collect();
            sorted.sort_by_key(|r| r.resource);
            sorted
                .iter()
                .map(|r| {
                    let mut verbs: Vec<&str> = r.verbs.to_vec();
                    verbs.sort_unstable();
                    let mut value = json!({"name": r.resource, "singularName": r.kind.to_lowercase(), "namespaced": r.namespaced, "kind": r.kind, "verbs": verbs});
                    if r.response_group != group || r.response_version != version {
                        value["group"] = json!(r.response_group);
                        value["version"] = json!(r.response_version);
                    }
                    let short_names = short_names_for(group, r.resource);
                    if !short_names.is_empty() {
                        value["shortNames"] = json!(short_names);
                    }
                    let categories = categories_for(group, r.resource);
                    if !categories.is_empty() {
                        value["categories"] = json!(categories);
                    }
                    value
                })
                .collect()
        })
        .unwrap_or_default();
    list.extend(dynamic.iter().map(|r| crd_api_resource_value(r)));
    list.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
    Some(json!({
        "kind": "APIResourceList",
        "apiVersion": "v1",
        "groupVersion": group_version,
        "resources": list,
    }))
}

/// Same shape [`api_resource_list`]'s own per-resource entries use, for
/// one CRD-provided resource — `CRD_VERBS` in place of a compiled
/// per-type verb list, since every CRD-backed resource genuinely
/// supports the same generic set (this module's own doc comment on
/// `CRD_VERBS`).
fn crd_api_resource_value(r: &DiscoverableResource) -> Value {
    let mut value = json!({"name": r.resource, "singularName": r.kind.to_lowercase(), "namespaced": r.namespaced, "kind": r.kind, "verbs": CRD_VERBS});
    if !r.short_names.is_empty() {
        value["shortNames"] = json!(r.short_names);
    }
    if !r.categories.is_empty() {
        value["categories"] = json!(r.categories);
    }
    value
}

/// Aggregated discovery v2 (`apidiscovery.k8s.io/v2`'s `APIGroupDiscoveryList`
/// — introduced 1.30, GA by 1.34): one request instead of the legacy
/// `/apis` + one `/apis/{group}/{version}` per group-version, real shape
/// confirmed directly against upstream's own
/// `staging/src/k8s.io/api/apidiscovery/v2/types.go`. Reachable over HTTP
/// via `listener::route_discovery`'s content negotiation: a client whose
/// `Accept` header asks for `as=APIGroupDiscoveryList;v=v2;
/// g=apidiscovery.k8s.io` gets this shape at `/api`/`/apis` instead of the
/// legacy `APIVersions`/`APIGroupList`.
///
/// `freshness` is always `"Current"`: this build has no aggregation layer
/// (Group L) merging discovery from multiple backing apiservers yet, so
/// there is no scenario in which this process's own discovery could be
/// stale relative to itself.
pub fn api_group_discovery_list() -> Value {
    let groups = group_version_map();
    let items: Vec<Value> = groups.keys().map(|group| group_discovery_value(group)).collect();
    json!({
        "kind": "APIGroupDiscoveryList",
        "apiVersion": "apidiscovery.k8s.io/v2",
        "metadata": {},
        "items": items,
    })
}

/// The core (groupless) group's own aggregated discovery document, the
/// `/api` analogue of [`api_group_discovery_list`] — a single-item list,
/// matching real upstream's own posture that `/api`'s aggregated response
/// still carries the `APIGroupDiscoveryList` envelope, just scoped to the
/// one group named `""`.
pub fn api_v1_group_discovery_list() -> Value {
    json!({
        "kind": "APIGroupDiscoveryList",
        "apiVersion": "apidiscovery.k8s.io/v2",
        "metadata": {},
        "items": [group_discovery_value("")],
    })
}

fn group_discovery_value(group: &str) -> Value {
    let mut versions: Vec<&str> = codegen::openapi_meta::DISCOVERY_GVKS.iter().filter(|g| g.group == group).map(|g| g.version).collect();
    versions.sort_unstable();
    versions.dedup();
    sort_versions_most_preferred_first(&mut versions);

    let version_values: Vec<Value> = versions
        .iter()
        .map(|version| {
            let resources = codegen::api_resources_by_group_version().get(&(group, *version));
            let mut sorted: Vec<&codegen::api_resources::ApiResource> = resources.map(|r| r.iter().copied().collect()).unwrap_or_default();
            sorted.sort_by_key(|r| r.resource);
            let resource_values: Vec<Value> = sorted.iter().map(|r| api_resource_discovery_value(r)).collect();
            json!({
                "version": version,
                "resources": resource_values,
                "freshness": "Current",
            })
        })
        .collect();

    json!({
        "metadata": {"name": group},
        "versions": version_values,
    })
}

/// The dynamic counterpart to [`api_group_discovery_list`] — see
/// [`merged_group_version_map`]'s own doc comment for where `crds`
/// comes from.
pub fn api_group_discovery_list_with_crds(crds: &[DiscoverableResource], aggregated: &[(String, String)]) -> Value {
    let groups = merged_group_version_map(crds, aggregated);
    let items: Vec<Value> = groups.keys().map(|group| group_discovery_value_with_crds(group, crds, aggregated)).collect();
    json!({
        "kind": "APIGroupDiscoveryList",
        "apiVersion": "apidiscovery.k8s.io/v2",
        "metadata": {},
        "items": items,
    })
}

/// The dynamic counterpart to [`api_v1_group_discovery_list`] — the core
/// group never has any CRDs of its own (a CRD's `spec.group` is never
/// empty, real upstream's own CRD validation requires a non-empty
/// group), so this exists only for symmetry with every other
/// `*_with_crds` function, not because `crds` ever actually changes its
/// output.
pub fn api_v1_group_discovery_list_with_crds() -> Value {
    api_v1_group_discovery_list()
}

/// Same shape [`group_discovery_value`] builds, with every
/// `(group, version)` resource list also merged with whatever `crds`
/// provides for that exact group. `aggregated`'s own group/versions
/// appear too (same [`merged_group_version_map`] input as the legacy
/// shape), each with an empty `resources` list here — this pure builder
/// has no I/O of its own to actually fetch an aggregated backend's real
/// resource list. That's not left undone, though: a *plain* `GET
/// /apis/{group}/{version}` for one of these groups is caught earlier,
/// in `server::listener::handle`, and answered with a real live proxied
/// fetch to the backend's own discovery endpoint instead of ever
/// reaching this function — this empty-`resources` shape is only what
/// `apidiscovery.k8s.io/v2`'s own aggregated multi-group listing
/// (`/apis` with `Accept: application/json;as=APIGroupDiscoveryList...`)
/// shows for an aggregated group, since that one request can't proxy to
/// N different backends at once the way a single-group request can.
fn group_discovery_value_with_crds(group: &str, crds: &[DiscoverableResource], aggregated: &[(String, String)]) -> Value {
    let versions = merged_group_version_map(crds, aggregated).remove(group).unwrap_or_default();
    let version_values: Vec<Value> = versions
        .iter()
        .map(|version| {
            let resources = codegen::api_resources_by_group_version().get(&(group, version.as_str()));
            let mut sorted: Vec<&codegen::api_resources::ApiResource> = resources.map(|r| r.iter().copied().collect()).unwrap_or_default();
            sorted.sort_by_key(|r| r.resource);
            let mut resource_values: Vec<Value> = sorted.iter().map(|r| api_resource_discovery_value(r)).collect();
            resource_values.extend(crds.iter().filter(|r| r.group == group && &r.version == version).map(crd_resource_discovery_value));
            json!({
                "version": version,
                "resources": resource_values,
                "freshness": "Current",
            })
        })
        .collect();

    json!({
        "metadata": {"name": group},
        "versions": version_values,
    })
}

/// Same shape [`api_resource_discovery_value`] builds, for one
/// CRD-provided resource — `CRD_VERBS` in place of a compiled per-type
/// verb list, same reasoning [`crd_api_resource_value`] already states.
fn crd_resource_discovery_value(r: &DiscoverableResource) -> Value {
    let mut value = json!({
        "resource": r.resource,
        "responseKind": {"group": r.group, "version": r.version, "kind": r.kind},
        "scope": if r.namespaced { "Namespaced" } else { "Cluster" },
        "singularResource": r.kind.to_lowercase(),
        "verbs": CRD_VERBS,
    });
    if !r.short_names.is_empty() {
        value["shortNames"] = json!(r.short_names);
    }
    if !r.categories.is_empty() {
        value["categories"] = json!(r.categories);
    }
    value
}

fn api_resource_discovery_value(r: &codegen::api_resources::ApiResource) -> Value {
    let mut verbs: Vec<&str> = r.verbs.to_vec();
    verbs.sort_unstable();
    let mut value = json!({
        "resource": r.resource,
        "responseKind": {"group": r.response_group, "version": r.response_version, "kind": r.kind},
        "scope": if r.namespaced { "Namespaced" } else { "Cluster" },
        "singularResource": r.kind.to_lowercase(),
        "verbs": verbs,
    });
    let short_names = short_names_for(r.response_group, r.resource);
    if !short_names.is_empty() {
        value["shortNames"] = json!(short_names);
    }
    let categories = categories_for(r.response_group, r.resource);
    if !categories.is_empty() {
        value["categories"] = json!(categories);
    }
    value
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
        assert_eq!(v["apiVersion"], "v1");
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
    fn non_resource_schema_gvks_do_not_create_phantom_api_groups() {
        assert!(api_group("admission.k8s.io").is_none());
        let group_list = api_group_list();
        let groups = group_list["groups"].as_array().unwrap();
        assert!(groups.iter().all(|group| group["name"] != "admission.k8s.io"));
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
        assert_eq!(pods["categories"], json!(["all"]));
    }

    #[test]
    fn api_resource_list_includes_standard_short_names() {
        let list = api_resource_list("", "v1").expect("core/v1 should be served");
        let resources = list["resources"].as_array().unwrap();
        let pvc = resources
            .iter()
            .find(|resource| resource["name"] == "persistentvolumeclaims")
            .expect("persistentvolumeclaims should be listed");
        assert_eq!(pvc["shortNames"], json!(["pvc"]));
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

    #[test]
    fn aggregated_discovery_list_has_the_real_kind_and_apiversion() {
        let list = api_group_discovery_list();
        assert_eq!(list["kind"], "APIGroupDiscoveryList");
        assert_eq!(list["apiVersion"], "apidiscovery.k8s.io/v2");
    }

    #[test]
    fn aggregated_discovery_list_never_contains_the_core_group_and_v1_group_discovery_list_is_only_the_core_group() {
        let list = api_group_discovery_list();
        let items = list["items"].as_array().unwrap();
        assert!(items.iter().all(|g| g["metadata"]["name"] != ""), "the core group belongs to the /api document, not /apis");

        let core = api_v1_group_discovery_list();
        let core_items = core["items"].as_array().unwrap();
        assert_eq!(core_items.len(), 1);
        assert_eq!(core_items[0]["metadata"]["name"], "");
    }

    #[test]
    fn aggregated_discovery_resource_entries_carry_a_real_response_kind_and_scope() {
        let core = api_v1_group_discovery_list();
        let versions = core["items"][0]["versions"].as_array().unwrap();
        let v1 = versions.iter().find(|v| v["version"] == "v1").expect("core v1 should be present");
        let resources = v1["resources"].as_array().unwrap();
        let pods = resources.iter().find(|r| r["resource"] == "pods").expect("pods should be discoverable");
        assert_eq!(pods["responseKind"], json!({"group": "", "version": "v1", "kind": "Pod"}));
        assert_eq!(pods["scope"], "Namespaced");
        assert_eq!(pods["singularResource"], "pod");
        assert_eq!(v1["freshness"], "Current");
    }

    #[test]
    fn aggregated_discovery_versions_are_sorted_most_preferred_first() {
        let list = api_group_discovery_list();
        let items = list["items"].as_array().unwrap();
        let resource_group = items.iter().find(|g| g["metadata"]["name"] == "resource.k8s.io").expect("resource.k8s.io should be present");
        let versions: Vec<&str> = resource_group["versions"].as_array().unwrap().iter().map(|v| v["version"].as_str().unwrap()).collect();
        assert!(versions.len() > 1, "expected a genuinely multi-version group, got {versions:?}");
        assert_eq!(versions[0], "v1", "the GA version must be preferred over any beta");
    }

    fn a_widget_crd_resource() -> DiscoverableResource {
        DiscoverableResource {
            group: "example.com".to_string(),
            version: "v1".to_string(),
            resource: "widgets".to_string(),
            kind: "Widget".to_string(),
            namespaced: true,
            short_names: vec![],
            categories: vec!["widgets".to_string()],
        }
    }

    #[test]
    fn a_crd_group_with_no_static_counterpart_appears_in_the_group_list() {
        let crds = [a_widget_crd_resource()];
        let list = api_group_list_with_crds(&crds, &[]);
        let names: Vec<&str> = list["groups"].as_array().unwrap().iter().map(|g| g["name"].as_str().unwrap()).collect();
        assert!(names.contains(&"example.com"), "expected the CRD's own group to be discoverable, got {names:?}");
        // Every static group must still be present too -- the merge adds,
        // it never replaces.
        assert!(names.contains(&"apps"));
    }

    #[test]
    fn a_crd_only_group_document_carries_its_own_version_as_preferred() {
        let crds = [a_widget_crd_resource()];
        let group = api_group_with_crds("example.com", &crds, &[]).expect("the CRD's own group must resolve");
        assert_eq!(group["preferredVersion"]["version"], "v1");
        assert_eq!(group["preferredVersion"]["groupVersion"], "example.com/v1");
    }

    #[test]
    fn a_group_neither_static_nor_crd_provided_is_still_none() {
        let crds = [a_widget_crd_resource()];
        assert!(api_group_with_crds("totally.made.up", &crds, &[]).is_none());
    }

    /// Group L Phase 3: an aggregated `APIService`'s own group/version
    /// shows up the same way a CRD's does -- the third real merge input.
    #[test]
    fn an_aggregated_group_with_no_static_or_crd_counterpart_appears_in_the_group_list() {
        let aggregated = [("metrics.k8s.io".to_string(), "v1beta1".to_string())];
        let list = api_group_list_with_crds(&[], &aggregated);
        let names: Vec<&str> = list["groups"].as_array().unwrap().iter().map(|g| g["name"].as_str().unwrap()).collect();
        assert!(names.contains(&"metrics.k8s.io"), "expected the aggregated group to be discoverable, got {names:?}");
    }

    #[test]
    fn an_aggregated_only_group_document_carries_its_own_version_as_preferred() {
        let aggregated = [("metrics.k8s.io".to_string(), "v1beta1".to_string())];
        let group = api_group_with_crds("metrics.k8s.io", &[], &aggregated).expect("the aggregated group must resolve");
        assert_eq!(group["preferredVersion"]["version"], "v1beta1");
        assert_eq!(group["preferredVersion"]["groupVersion"], "metrics.k8s.io/v1beta1");
    }

    #[test]
    fn a_crd_and_an_aggregated_group_both_merge_alongside_the_static_table() {
        let crds = [a_widget_crd_resource()];
        let aggregated = [("metrics.k8s.io".to_string(), "v1beta1".to_string())];
        let list = api_group_list_with_crds(&crds, &aggregated);
        let names: Vec<&str> = list["groups"].as_array().unwrap().iter().map(|g| g["name"].as_str().unwrap()).collect();
        assert!(names.contains(&"example.com"));
        assert!(names.contains(&"metrics.k8s.io"));
        assert!(names.contains(&"apps"));
    }

    #[test]
    fn a_crd_resource_appears_in_its_own_group_versions_resource_list_with_every_real_generic_verb() {
        let crds = [a_widget_crd_resource()];
        let list = api_resource_list_with_crds("example.com", "v1", &crds).expect("example.com/v1 must resolve dynamically");
        assert_eq!(list["groupVersion"], "example.com/v1");
        let resources = list["resources"].as_array().unwrap();
        let widgets = resources.iter().find(|r| r["name"] == "widgets").expect("widgets should be listed");
        assert_eq!(widgets["kind"], "Widget");
        assert_eq!(widgets["namespaced"], true);
        assert_eq!(widgets["singularName"], "widget");
        assert_eq!(widgets["categories"], json!(["widgets"]));
        let verbs: Vec<&str> = widgets["verbs"].as_array().unwrap().iter().map(|v| v.as_str().unwrap()).collect();
        for expected in ["create", "get", "list", "update", "patch", "delete", "deletecollection", "watch"] {
            assert!(verbs.contains(&expected), "expected {expected:?} among {verbs:?}");
        }
    }

    #[test]
    fn a_crd_resource_still_sits_alongside_static_resources_in_a_shared_group_version() {
        // A pathological but real-shaped case: a CRD happens to share a
        // group+version with a static resource this build already knows
        // about (unlikely for a real cluster's own core/apps groups, but
        // the merge logic itself must not silently drop either side).
        let crds = [DiscoverableResource {
            group: "apps".to_string(),
            version: "v1".to_string(),
            resource: "widgets".to_string(),
            kind: "Widget".to_string(),
            namespaced: true,
            short_names: vec![],
            categories: vec![],
        }];
        let list = api_resource_list_with_crds("apps", "v1", &crds).expect("apps/v1 must still resolve");
        let names: Vec<&str> = list["resources"].as_array().unwrap().iter().map(|r| r["name"].as_str().unwrap()).collect();
        assert!(names.contains(&"deployments"), "the static apps/v1 resources must still be present");
        assert!(names.contains(&"widgets"), "the CRD-provided resource must be merged in too");
    }

    #[test]
    fn a_group_version_provided_only_by_a_crd_is_none_without_it_and_some_with_it() {
        assert!(api_resource_list_with_crds("example.com", "v1", &[]).is_none());
        let crds = [a_widget_crd_resource()];
        assert!(api_resource_list_with_crds("example.com", "v1", &crds).is_some());
    }

    #[test]
    fn aggregated_discovery_merges_a_crd_group_too() {
        let crds = [a_widget_crd_resource()];
        let list = api_group_discovery_list_with_crds(&crds, &[]);
        let items = list["items"].as_array().unwrap();
        let group = items.iter().find(|g| g["metadata"]["name"] == "example.com").expect("example.com should be discoverable");
        let v1 = group["versions"].as_array().unwrap().iter().find(|v| v["version"] == "v1").expect("v1 should be present");
        let widgets = v1["resources"].as_array().unwrap().iter().find(|r| r["resource"] == "widgets").expect("widgets should be discoverable");
        assert_eq!(widgets["responseKind"], json!({"group": "example.com", "version": "v1", "kind": "Widget"}));
        assert_eq!(widgets["scope"], "Namespaced");
    }

    /// Group L Phase 3's discovery merge: an aggregated group/version now
    /// shows up in the v2 shape too, matching the legacy shape's own merge.
    /// The initial group-level merge has an empty `resources` list because
    /// the backend's live resource enumeration is fetched separately for
    /// the exact `/apis/{group}/{version}` request.
    #[test]
    fn aggregated_discovery_merges_an_aggregated_apiservice_group_too() {
        let aggregated = [("metrics.k8s.io".to_string(), "v1beta1".to_string())];
        let list = api_group_discovery_list_with_crds(&[], &aggregated);
        let items = list["items"].as_array().unwrap();
        let group = items.iter().find(|g| g["metadata"]["name"] == "metrics.k8s.io").expect("metrics.k8s.io should be discoverable");
        let v1beta1 = group["versions"].as_array().unwrap().iter().find(|v| v["version"] == "v1beta1").expect("v1beta1 should be present");
        assert_eq!(v1beta1["resources"], json!([]), "an aggregated group's own resources aren't known statically yet");
    }
}
