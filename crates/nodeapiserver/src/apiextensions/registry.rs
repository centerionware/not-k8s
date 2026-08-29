//! Dynamic `(group, version, resource)` resolution against *stored*
//! `CustomResourceDefinition` documents — the CRD analogue of
//! `server::rest::resolve_kind`'s static, build-time-generated table.
//! Pure decode/lookup logic only, no I/O: `server::rest` is the caller
//! that has a real `StorageClient` to fetch the CRD list from
//! (`resolve_resource`, this crate's own single fallback consulted only
//! after the static table misses).
//!
//! Every function here reads a `CustomResourceDefinition` the same way
//! `server::rest` already decodes one off the wire — as a plain
//! `serde_json::Value` — rather than the typed
//! `k8s_openapi::apiextensions_apiserver::...::CustomResourceDefinition`
//! struct: this module has to `Value`-walk `spec.versions[].schema.
//! openAPIV3Schema` regardless (an arbitrary, operator-defined OpenAPI
//! v3 schema tree k8s-openapi has no static type for), so reading the
//! rest of the document the same way keeps one access pattern instead of
//! two.

use serde_json::Value;

/// The conversion webhook configuration attached to a served CRD. The
/// storage version is where objects are kept; the webhook is called only
/// when a request or response crosses that version boundary.
#[derive(Debug, Clone, PartialEq)]
pub struct ConversionWebhook {
    pub storage_version: String,
    pub client_config: Value,
    pub review_versions: Vec<String>,
}

/// What a stored, `Established` CRD resolves `(group, version, resource)`
/// to — everything `server::rest`'s generic verb dispatch needs that it
/// would otherwise get from the static `resolve_kind`/`schema_for_gvk`
/// pair.
#[derive(Debug, Clone, PartialEq)]
pub struct CrdResource {
    pub kind: String,
    pub namespaced: bool,
    /// `spec.versions[].schema.openAPIV3Schema` for the matched version,
    /// if the CRD carries one (real upstream requires a schema on every
    /// version served via `apiextensions.k8s.io/v1` — `None` here is
    /// this build's own defensive fallback for a malformed/legacy
    /// document, not a real case a validated CRD produces). Consulted by
    /// `apiextensions::schema_defaults` for structural-schema defaulting and
    /// by the request-side structural validation walk.
    pub open_api_schema: Option<Value>,
    /// The storage version's schema, used to revalidate the result of a
    /// conversion webhook before it is persisted. A webhook can return a
    /// shape that is valid for the requested version but not for storage.
    pub storage_open_api_schema: Option<Value>,
    /// Whether the matched version's own `subresources.status` is
    /// present — real upstream only serves `GET`/`PUT`/`PATCH .../status`
    /// for a CRD version that opts in this way (`spec.versions[].
    /// subresources: {status: {}}`); a CRD with no such key has no
    /// `status` subresource at all, matching every other resource that
    /// simply doesn't have one (a real `404`, not a silent fallthrough
    /// to the main object). Only the key's *presence* matters — real
    /// upstream's own `CustomResourceSubresourceStatus` carries no
    /// fields of its own to configure (an empty object `{}` is the only
    /// valid non-absent value).
    pub has_status_subresource: bool,
    /// Present when this CRD declares `spec.conversion.strategy: Webhook`.
    pub conversion_webhook: Option<ConversionWebhook>,
}

/// Real upstream's own `Established` condition
/// (`pkg/apiserver/apiserver.go`'s `crdHandler` only routes to a CRD's
/// storage once this is `"True"`) — this build sets it synchronously in
/// `apiextensions::conditions` right on `CREATE`/`UPDATE` of the CRD
/// itself rather than through a separate async establishing controller
/// (see that module's own doc comment for why), but the *check* here is
/// the same either way: a stored document either carries the condition
/// or it doesn't.
pub fn is_established(crd: &Value) -> bool {
    crd.pointer("/status/conditions")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .any(|c| c.get("type").and_then(Value::as_str) == Some("Established") && c.get("status").and_then(Value::as_str) == Some("True"))
}

/// The one `spec.versions[]` entry matching `version` that's also
/// actually served (`served: true` -- a CRD can define a version without
/// serving it, real upstream's own deprecation mechanism). `None` covers
/// both "no such version defined" and "defined but not served" with the
/// same `resolve` outcome (a real 404 either way from the caller's own
/// perspective).
fn served_version<'a>(crd: &'a Value, version: &str) -> Option<&'a Value> {
    crd.pointer("/spec/versions")?
        .as_array()?
        .iter()
        .find(|v| v.get("name").and_then(Value::as_str) == Some(version) && v.get("served").and_then(Value::as_bool) == Some(true))
}

/// Resolves `(group, version, resource)` against one decoded
/// `CustomResourceDefinition` document — `None` if this particular CRD
/// doesn't define that triple at all, isn't `Established` yet, or
/// doesn't serve that version.
pub fn resolve(crd: &Value, group: &str, version: &str, resource: &str) -> Option<CrdResource> {
    if crd.pointer("/spec/group").and_then(Value::as_str) != Some(group) {
        return None;
    }
    if crd.pointer("/spec/names/plural").and_then(Value::as_str) != Some(resource) {
        return None;
    }
    if !is_established(crd) {
        return None;
    }
    let matched_version = served_version(crd, version)?;
    let kind = crd.pointer("/spec/names/kind").and_then(Value::as_str)?.to_string();
    let namespaced = crd.pointer("/spec/scope").and_then(Value::as_str) == Some("Namespaced");
    let open_api_schema = matched_version.pointer("/schema/openAPIV3Schema").cloned();
    let storage_open_api_schema = crd
        .pointer("/spec/versions")
        .and_then(Value::as_array)
        .and_then(|versions| versions.iter().find(|version| version.get("storage").and_then(Value::as_bool) == Some(true)))
        .and_then(|version| version.pointer("/schema/openAPIV3Schema"))
        .cloned();
    let has_status_subresource = matched_version.pointer("/subresources/status").is_some();
    let conversion_webhook = conversion_webhook(crd);
    Some(CrdResource { kind, namespaced, open_api_schema, storage_open_api_schema, has_status_subresource, conversion_webhook })
}

fn conversion_webhook(crd: &Value) -> Option<ConversionWebhook> {
    let storage_version = crd
        .pointer("/spec/versions")
        .and_then(Value::as_array)?
        .iter()
        .find(|version| version.get("storage").and_then(Value::as_bool) == Some(true))
        .and_then(|version| version.get("name"))
        .and_then(Value::as_str)?
        .to_string();
    let conversion = crd.get("spec")?.get("conversion")?;
    if conversion.get("strategy").and_then(Value::as_str) != Some("Webhook") {
        return None;
    }
    let webhook = conversion.get("webhook")?;
    let client_config = webhook.get("clientConfig")?.clone();
    let review_versions = webhook
        .get("conversionReviewVersions")
        .and_then(Value::as_array)?
        .iter()
        .filter_map(Value::as_str)
        .map(str::to_string)
        .collect::<Vec<_>>();
    if review_versions.is_empty() {
        return None;
    }
    Some(ConversionWebhook { storage_version, client_config, review_versions })
}

/// Scans every CRD in `crds` for one that resolves `(group, version,
/// resource)` — the dynamic counterpart to `server::rest::resolve_kind`,
/// consulted only after that static table misses. Real upstream's own
/// CRD naming-conflict rules (enforced by `apiextensions::conditions` on
/// write, see its own doc comment) mean at most one *established* CRD
/// should ever match here in practice; on the pathological case of two
/// somehow both matching (a bug elsewhere, not a state this function can
/// itself prevent), the first one found wins rather than this function
/// panicking or erroring.
pub fn resolve_in<'a>(crds: impl IntoIterator<Item = &'a Value>, group: &str, version: &str, resource: &str) -> Option<CrdResource> {
    crds.into_iter().find_map(|crd| resolve(crd, group, version, resource))
}

/// One `(group, version, resource)` a served, `Established` CRD makes
/// discoverable — `server::discovery`'s own dynamic-merge counterpart to
/// Group A's static `codegen::openapi_meta::DISCOVERY_GVKS` table.
#[derive(Debug, Clone, PartialEq)]
pub struct DiscoverableResource {
    pub group: String,
    pub version: String,
    pub resource: String,
    pub kind: String,
    pub namespaced: bool,
}

/// Every `(group, version, resource)` triple every served, `Established`
/// CRD in `crds` provides — one entry per served version, since real
/// upstream's own discovery lists a resource once per version it serves
/// (`/apis/{group}/{version}` is scoped to exactly one version already,
/// same as this build's own static `api_resource_list`). Reuses
/// [`resolve`] itself for each candidate triple rather than re-deriving
/// the same served/`Established` logic a second way, so there's exactly
/// one place that logic lives.
pub fn discoverable_resources<'a>(crds: impl IntoIterator<Item = &'a Value>) -> Vec<DiscoverableResource> {
    let mut out = Vec::new();
    for crd in crds {
        let (Some(group), Some(resource), Some(versions)) =
            (crd.pointer("/spec/group").and_then(Value::as_str), crd.pointer("/spec/names/plural").and_then(Value::as_str), crd.pointer("/spec/versions").and_then(Value::as_array))
        else {
            continue;
        };
        for v in versions {
            let Some(version) = v.get("name").and_then(Value::as_str) else { continue };
            if let Some(resolved) = resolve(crd, group, version, resource) {
                out.push(DiscoverableResource { group: group.to_string(), version: version.to_string(), resource: resource.to_string(), kind: resolved.kind, namespaced: resolved.namespaced });
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn established_crd() -> Value {
        json!({
            "spec": {
                "group": "example.com",
                "scope": "Namespaced",
                "names": {"plural": "widgets", "singular": "widget", "kind": "Widget", "listKind": "WidgetList"},
                "versions": [
                    {"name": "v1", "served": true, "storage": true, "schema": {"openAPIV3Schema": {"type": "object", "properties": {"spec": {"type": "object"}}}}},
                    {"name": "v1beta1", "served": false, "storage": false},
                ],
            },
            "status": {
                "conditions": [
                    {"type": "NamesAccepted", "status": "True"},
                    {"type": "Established", "status": "True"},
                ],
            },
        })
    }

    #[test]
    fn resolves_a_served_version_of_an_established_crd() {
        let crd = established_crd();
        let resolved = resolve(&crd, "example.com", "v1", "widgets").expect("should resolve");
        assert_eq!(resolved.kind, "Widget");
        assert!(resolved.namespaced);
        assert!(resolved.open_api_schema.is_some());
        assert!(resolved.storage_open_api_schema.is_some());
        assert!(!resolved.has_status_subresource, "established_crd()'s own fixture never declares subresources.status");
    }

    #[test]
    fn a_version_declaring_subresources_status_reports_it() {
        let mut crd = established_crd();
        crd["spec"]["versions"][0]["subresources"] = json!({"status": {}});
        let resolved = resolve(&crd, "example.com", "v1", "widgets").expect("should resolve");
        assert!(resolved.has_status_subresource);
    }

    #[test]
    fn a_defined_but_unserved_version_does_not_resolve() {
        let crd = established_crd();
        assert_eq!(resolve(&crd, "example.com", "v1beta1", "widgets"), None);
    }

    #[test]
    fn a_wrong_group_or_resource_does_not_resolve() {
        let crd = established_crd();
        assert_eq!(resolve(&crd, "other.com", "v1", "widgets"), None);
        assert_eq!(resolve(&crd, "example.com", "v1", "gizmos"), None);
    }

    #[test]
    fn a_crd_not_yet_established_does_not_resolve() {
        let mut crd = established_crd();
        crd["status"]["conditions"] = json!([{"type": "NamesAccepted", "status": "True"}]);
        assert_eq!(resolve(&crd, "example.com", "v1", "widgets"), None);
    }

    #[test]
    fn cluster_scoped_crds_report_namespaced_false() {
        let mut crd = established_crd();
        crd["spec"]["scope"] = json!("Cluster");
        let resolved = resolve(&crd, "example.com", "v1", "widgets").expect("should resolve");
        assert!(!resolved.namespaced);
    }

    #[test]
    fn resolves_a_crd_conversion_webhook_and_storage_version() {
        let mut crd = established_crd();
        crd["spec"]["conversion"] = json!({
            "strategy": "Webhook",
            "webhook": {
                "conversionReviewVersions": ["v1"],
                "clientConfig": {"url": "https://converter.example/convert"}
            }
        });
        let resolved = resolve(&crd, "example.com", "v1", "widgets").expect("should resolve");
        let conversion = resolved.conversion_webhook.expect("webhook conversion should be retained");
        assert_eq!(conversion.storage_version, "v1");
        assert_eq!(conversion.review_versions, ["v1"]);
        assert_eq!(conversion.client_config["url"], "https://converter.example/convert");
    }

    #[test]
    fn resolve_in_finds_the_matching_crd_among_several() {
        let other = json!({"spec": {"group": "other.com", "scope": "Namespaced", "names": {"plural": "things", "kind": "Thing"}, "versions": []}, "status": {}});
        let crds = vec![other, established_crd()];
        let resolved = resolve_in(crds.iter(), "example.com", "v1", "widgets").expect("should resolve");
        assert_eq!(resolved.kind, "Widget");
    }

    #[test]
    fn resolve_in_returns_none_when_nothing_matches() {
        let crds = vec![established_crd()];
        assert_eq!(resolve_in(crds.iter(), "nope.com", "v1", "widgets"), None);
    }

    #[test]
    fn discoverable_resources_lists_only_served_versions_of_established_crds() {
        let crds = vec![established_crd()];
        let resources = discoverable_resources(crds.iter());
        // v1 is served, v1beta1 isn't -- exactly one entry, not two.
        assert_eq!(resources.len(), 1);
        assert_eq!(resources[0], DiscoverableResource { group: "example.com".to_string(), version: "v1".to_string(), resource: "widgets".to_string(), kind: "Widget".to_string(), namespaced: true });
    }

    #[test]
    fn discoverable_resources_skips_a_crd_not_yet_established() {
        let mut crd = established_crd();
        crd["status"]["conditions"] = json!([]);
        assert!(discoverable_resources([&crd]).is_empty());
    }
}
