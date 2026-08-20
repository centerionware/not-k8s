//! The REST path grammar: turns an HTTP method + URL path + query string
//! into a [`RequestInfo`] — group/version/namespace/resource/name/
//! subresource/verb. Every other handler-chain stage (authn, authz, APF,
//! admission, the REST dispatch itself, Group E's own eventual listener)
//! reads this rather than re-parsing the path — same reason upstream
//! computes it once and threads it through `context.Context`.
//!
//! A faithful, line-by-line port of upstream's own
//! `staging/src/k8s.io/apiserver/pkg/endpoints/request/requestinfo.go`
//! (`RequestInfoFactory.NewRequestInfo`, `release-1.34`) — not a
//! reimplementation from the shape of a few example paths. Every branch
//! below traces back to a specific line there; the doc comment on
//! `NewRequestInfo` itself supplied the example paths this module's own
//! tests use, so the tests are upstream's own documented contract, not
//! invented cases.
//!
//! Known, named simplifications against the real thing:
//! - No `AuthorizeWithSelectors` feature gate — field/label selectors are
//!   always captured for `list`/`watch`/`deletecollection`, matching what
//!   that gate does once *enabled* (real upstream still guards it because
//!   this project has no larger feature-gate system yet to hang a gate on).
//! - No `ListOptions` field-selector-driven `metadata.name` extraction for
//!   a `list` whose selector happens to pin one name exactly — real
//!   upstream's `NewRequestInfo` does this via a full
//!   `metainternalversion.ListOptions` decode; this crate has no such type
//!   yet (Group F). `Name` on a nameless `list`/`watch` stays empty here.

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RequestInfo {
    pub is_resource_request: bool,
    pub path: String,
    /// The kube verb (`get`, `list`, `watch`, `create`, `update`, `patch`,
    /// `delete`, `deletecollection`, `proxy`, ...) — **not** the HTTP
    /// method. For a non-resource request this is the lowercased HTTP
    /// method instead, matching upstream's own fallback.
    pub verb: String,
    pub api_prefix: String,
    pub api_group: String,
    pub api_version: String,
    pub namespace: String,
    pub resource: String,
    pub subresource: String,
    pub name: String,
    pub parts: Vec<String>,
    pub field_selector: String,
    pub label_selector: String,
}

const API_PREFIXES: [&str; 2] = ["api", "apis"];
const GROUPLESS_API_PREFIXES: [&str; 1] = ["api"];
const SPECIAL_VERBS: [&str; 2] = ["proxy", "watch"];
const SPECIAL_VERBS_NO_SUBRESOURCES: [&str; 1] = ["proxy"];
const NAMESPACE_SUBRESOURCES: [&str; 2] = ["status", "finalize"];
const VERBS_WITH_SELECTORS: [&str; 3] = ["list", "watch", "deletecollection"];

/// `method` is the HTTP method (`"GET"`, `"POST"`, ...); `path` is the
/// request path (`/apis/apps/v1/namespaces/default/deployments/my-app`);
/// `query` is the raw query string (no leading `?`), form-encoded
/// (`fieldSelector=metadata.name%3Dfoo&watch=true`).
pub fn parse(method: &str, path: &str, query: &str) -> RequestInfo {
    let mut info = RequestInfo { is_resource_request: false, path: path.to_string(), verb: method.to_ascii_lowercase(), ..Default::default() };

    let mut parts = split_path(path);
    if parts.len() < 3 {
        return info;
    }
    if !API_PREFIXES.contains(&parts[0].as_str()) {
        return info;
    }
    info.api_prefix = parts.remove(0);

    if !GROUPLESS_API_PREFIXES.contains(&info.api_prefix.as_str()) {
        // One part (the prefix) has already been consumed, so this is
        // really "do we have three parts left" (group, version, and at
        // least one more).
        if parts.len() < 3 {
            return info;
        }
        info.api_group = parts.remove(0);
    }

    info.is_resource_request = true;
    info.api_version = parts.remove(0);

    // /{specialVerb}/* — /watch/... and /proxy/...
    let mut verb_via_path_prefix = false;
    if !parts.is_empty() && SPECIAL_VERBS.contains(&parts[0].as_str()) {
        if parts.len() < 2 {
            // Upstream returns an error here; this crate has nothing yet
            // to hand that error to (Group E's actual handler chain), so
            // the caller sees an IsResourceRequest=true RequestInfo with
            // an empty resource/verb instead — a request shaped like
            // `/apis/apps/v1/watch` with nothing after it is nonsensical
            // either way and every real client always sends something
            // after the special verb.
            info.verb = parts.remove(0);
            info.parts = parts;
            return info;
        }
        info.verb = parts.remove(0);
        verb_via_path_prefix = true;
    } else {
        info.verb = match method.to_ascii_uppercase().as_str() {
            "POST" => "create",
            "GET" | "HEAD" => "get",
            "PUT" => "update",
            "PATCH" => "patch",
            "DELETE" => "delete",
            _ => "",
        }
        .to_string();
    }

    // /namespaces/{namespace}/{kind}/*, adjusted to be relative to kind.
    if parts.first().map(String::as_str) == Some("namespaces") {
        if parts.len() > 1 {
            info.namespace = parts[1].clone();
            if parts.len() > 2 && !NAMESPACE_SUBRESOURCES.contains(&parts[2].as_str()) {
                parts.drain(0..2);
            }
        }
    }
    // else: namespace stays "" — matches upstream's `metav1.NamespaceNone`.

    info.parts = parts.clone();

    // parts look like: resource/name/subresource/...(uninterpreted)
    let no_subresources = SPECIAL_VERBS_NO_SUBRESOURCES.contains(&info.verb.as_str());
    if parts.len() >= 3 && !no_subresources {
        info.subresource = parts[2].clone();
    }
    if parts.len() >= 2 {
        info.name = parts[1].clone();
    }
    if !parts.is_empty() {
        info.resource = parts[0].clone();
    }

    // No name + verb was "get" -> actually list or watch. Only the
    // *first* "watch" query value counts, matching upstream's own
    // `values[0]` — a repeated `watch=` param is not something any real
    // client sends, but the port should still be faithful to the letter
    // of the algorithm, not just its common case.
    if info.name.is_empty() && info.verb == "get" {
        let params = parse_query(query);
        let watch = params
            .iter()
            .find(|(k, _)| k == "watch")
            .map(|(_, v)| !matches!(v.to_ascii_lowercase().as_str(), "false" | "0"))
            .unwrap_or(false);
        info.verb = if watch { "watch".to_string() } else { "list".to_string() };
    }

    // No name + verb was "delete" -> deletecollection.
    if info.name.is_empty() && info.verb == "delete" {
        info.verb = "deletecollection".to_string();
    }

    if !verb_via_path_prefix && VERBS_WITH_SELECTORS.contains(&info.verb.as_str()) {
        let params = parse_query(query);
        if let Some((_, v)) = params.iter().find(|(k, _)| k == "fieldSelector") {
            info.field_selector = v.clone();
        }
        if let Some((_, v)) = params.iter().find(|(k, _)| k == "labelSelector") {
            info.label_selector = v.clone();
        }
    }

    info
}

fn split_path(path: &str) -> Vec<String> {
    let trimmed = path.trim_matches('/');
    if trimmed.is_empty() {
        return Vec::new();
    }
    trimmed.split('/').map(str::to_string).collect()
}

/// A small, self-contained `application/x-www-form-urlencoded` query
/// parser — not pulling in the `url` crate for this one well-scoped
/// surface, the same call this project's cron-schedule parser
/// (`crates/nodecontroller/src/cron_schedule.rs`) already made for a
/// similarly small grammar. Handles `%XX` percent-escapes and `+` as
/// space; does not validate UTF-8 strictly (falls back to the raw bytes
/// via `from_utf8_lossy` — a malformed escape degrading gracefully rather
/// than panicking is the right posture for untrusted request input).
fn parse_query(query: &str) -> Vec<(String, String)> {
    query
        .split('&')
        .filter(|s| !s.is_empty())
        .map(|pair| {
            let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
            (percent_decode(k), percent_decode(v))
        })
        .collect()
}

fn percent_decode(s: &str) -> String {
    let mut out = Vec::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok().and_then(|h| u8::from_str_radix(h, 16).ok());
                match hex {
                    Some(byte) => {
                        out.push(byte);
                        i += 3;
                    }
                    None => {
                        out.push(bytes[i]);
                        i += 1;
                    }
                }
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every path in this test is copied verbatim from
    /// `RequestInfoFactory.NewRequestInfo`'s own doc comment.
    #[test]
    fn resource_paths_from_upstreams_own_doc_comment() {
        let i = parse("GET", "/apis/apps/v1/namespaces", "");
        assert!(i.is_resource_request);
        assert_eq!((i.api_group.as_str(), i.api_version.as_str(), i.resource.as_str()), ("apps", "v1", "namespaces"));

        let i = parse("GET", "/api/v1/namespaces", "");
        assert_eq!((i.api_prefix.as_str(), i.api_version.as_str(), i.resource.as_str()), ("api", "v1", "namespaces"));
        assert!(i.api_group.is_empty(), "the core group has no group segment at all");

        let i = parse("GET", "/api/v1/namespaces/default", "");
        assert_eq!(i.resource, "namespaces");
        assert_eq!(i.name, "default");

        let i = parse("GET", "/api/v1/namespaces/default/pods", "");
        assert_eq!((i.namespace.as_str(), i.resource.as_str()), ("default", "pods"));
        assert_eq!(i.verb, "list");

        let i = parse("GET", "/api/v1/namespaces/default/pods/my-pod", "");
        assert_eq!((i.namespace.as_str(), i.resource.as_str(), i.name.as_str()), ("default", "pods", "my-pod"));
        assert_eq!(i.verb, "get");

        let i = parse("GET", "/api/v1/pods", "");
        assert_eq!(i.resource, "pods");
        assert_eq!(i.namespace, "", "cluster-scoped list has no namespace");

        let i = parse("GET", "/api/v1/pods/my-pod", "");
        assert_eq!(i.name, "my-pod");
    }

    #[test]
    fn special_verbs_without_subresources_proxy() {
        let i = parse("GET", "/api/v1/proxy/pods/my-pod", "");
        assert_eq!(i.verb, "proxy");
        assert_eq!(i.resource, "pods");
        assert_eq!(i.name, "my-pod");

        let i = parse("GET", "/api/v1/proxy/namespaces/default/pods/my-pod", "");
        assert_eq!(i.verb, "proxy");
        assert_eq!(i.namespace, "default");
        assert_eq!(i.resource, "pods");
        assert_eq!(i.name, "my-pod");
    }

    #[test]
    fn special_verbs_with_subresources_watch() {
        let i = parse("GET", "/api/v1/watch/pods", "");
        assert_eq!(i.verb, "watch");
        assert_eq!(i.resource, "pods");

        let i = parse("GET", "/api/v1/watch/namespaces/default/pods", "");
        assert_eq!(i.verb, "watch");
        assert_eq!(i.namespace, "default");
        assert_eq!(i.resource, "pods");
    }

    #[test]
    fn non_resource_paths_from_upstreams_own_doc_comment() {
        for path in ["/apis/apps/v1", "/apis/apps", "/apis", "/api/v1", "/api", "/healthz", "/"] {
            let i = parse("GET", path, "");
            assert!(!i.is_resource_request, "{path} must be a non-resource request");
        }
    }

    #[test]
    fn a_get_with_no_name_is_list_by_default_and_watch_with_the_query_param() {
        let i = parse("GET", "/api/v1/namespaces/default/pods", "");
        assert_eq!(i.verb, "list");

        let i = parse("GET", "/api/v1/namespaces/default/pods", "watch=true");
        assert_eq!(i.verb, "watch");

        let i = parse("GET", "/api/v1/namespaces/default/pods", "watch=false");
        assert_eq!(i.verb, "list", "watch=false must not be treated as watch");

        let i = parse("GET", "/api/v1/namespaces/default/pods", "watch=1");
        assert_eq!(i.verb, "watch");
    }

    #[test]
    fn a_delete_with_no_name_is_deletecollection() {
        let i = parse("DELETE", "/api/v1/namespaces/default/pods", "");
        assert_eq!(i.verb, "deletecollection");

        let i = parse("DELETE", "/api/v1/namespaces/default/pods/my-pod", "");
        assert_eq!(i.verb, "delete");
    }

    #[test]
    fn every_crud_http_method_maps_to_its_kube_verb() {
        assert_eq!(parse("POST", "/api/v1/namespaces/default/pods", "").verb, "create");
        assert_eq!(parse("PUT", "/api/v1/namespaces/default/pods/x", "").verb, "update");
        assert_eq!(parse("PATCH", "/api/v1/namespaces/default/pods/x", "").verb, "patch");
    }

    #[test]
    fn a_subresource_is_the_third_path_part_after_namespace_adjustment() {
        let i = parse("GET", "/api/v1/namespaces/default/pods/my-pod/status", "");
        assert_eq!(i.resource, "pods");
        assert_eq!(i.name, "my-pod");
        assert_eq!(i.subresource, "status");
        assert_eq!(i.verb, "get");
    }

    /// The parser's whole reason for the namespace-subresource distinction:
    /// `/namespaces/default/status` must not be parsed as "namespace 'default',
    /// resource 'status'" — `status` here is a *subresource of the Namespace
    /// object itself*, not a sibling resource.
    ///
    /// `Namespace` still ends up `"default"` here too — a genuine upstream
    /// quirk, traced through the real algorithm rather than assumed: the
    /// positional `namespaces/{X}/...` segment always fills `Namespace`
    /// with `X`, even when the request turns out to be for the
    /// cluster-scoped `Namespace` object itself. Real REST handlers know
    /// to ignore `Namespace` when `Resource == "namespaces"`; this parser
    /// doesn't need to special-case it away.
    #[test]
    fn a_namespace_subresource_is_not_treated_as_a_nested_resource() {
        let i = parse("GET", "/api/v1/namespaces/default/status", "");
        assert_eq!(i.resource, "namespaces");
        assert_eq!(i.name, "default");
        assert_eq!(i.subresource, "status");
        assert_eq!(i.namespace, "default", "positional quirk: Namespace is set even for the Namespace resource itself");
    }

    #[test]
    fn field_and_label_selectors_are_captured_for_list_watch_deletecollection() {
        let i = parse("GET", "/api/v1/pods", "fieldSelector=metadata.name%3Dfoo&labelSelector=app%3Dweb");
        assert_eq!(i.verb, "list");
        assert_eq!(i.field_selector, "metadata.name=foo");
        assert_eq!(i.label_selector, "app=web");
    }

    #[test]
    fn selectors_are_not_captured_for_a_verb_via_special_path_prefix() {
        // verb_via_path_prefix (e.g. /watch/...) is explicitly excluded —
        // matches upstream's own "!verbViaPathPrefix" guard.
        let i = parse("GET", "/api/v1/watch/pods", "fieldSelector=metadata.name%3Dfoo");
        assert_eq!(i.verb, "watch");
        assert!(i.field_selector.is_empty());
    }

    #[test]
    fn root_and_short_paths_are_non_resource_requests() {
        assert!(!parse("GET", "/", "").is_resource_request);
        assert!(!parse("GET", "/version", "").is_resource_request);
        assert!(!parse("GET", "/api", "").is_resource_request);
    }

    #[test]
    fn percent_decoding_handles_escapes_and_plus_as_space() {
        assert_eq!(percent_decode("a%3Db"), "a=b");
        assert_eq!(percent_decode("hello+world"), "hello world");
        assert_eq!(percent_decode("100%25"), "100%");
    }
}
