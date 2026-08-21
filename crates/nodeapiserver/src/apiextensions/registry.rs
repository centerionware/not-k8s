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
    /// `apiextensions::schema_defaults` for structural-schema defaulting;
    /// full type/required validation against it isn't done yet (Group
    /// K's own doc comment in `docs/APISERVER.md` names this honestly).
    pub open_api_schema: Option<Value>,
}

/// Real upstream's own `Established` condition
/// (`pkg/apiserver/apiserver.go`'s `crdHandler` only routes to a CRD's
/// storage once this is `"True"`) — this build sets it synchronously in
/// `apiextensions::conditions` right on `CREATE`/`UPDATE` of the CRD
/// itself rather than through a separate async establishing controller
/// (see that module's own doc comment for why), but the *check* here is
/// the same either way: a stored document either carries the condition
/// or it doesn't.
fn is_established(crd: &Value) -> bool {
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
    Some(CrdResource { kind, namespaced, open_api_schema })
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
}
