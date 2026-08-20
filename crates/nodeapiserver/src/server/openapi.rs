//! `/openapi/v3` — closes one item on Group E's own "not yet landed" list
//! (`docs/APISERVER.md`; `/openapi/v2` and `/version` remain, separate,
//! not this module's job).
//!
//! Every document served here is one of the files `Group A` already
//! vendors verbatim (`codegen::openapi_v3_docs`, built from
//! `vendor/openapi-spec/v3/*.json` by `build/openapi_serve.rs`) — this
//! module adds nothing of its own to a document's *content*, only the
//! root discovery index real `kubectl`/client-go expect at `/openapi/v3`
//! itself (real shape confirmed against upstream's own
//! `staging/src/k8s.io/apiserver/pkg/handler3/handler.go`: a `paths` map
//! from each servable path to a `{serverRelativeURL}` object carrying a
//! `?hash=` cache-busting query parameter) and the routing that serves an
//! individual document's raw bytes back for a request under
//! `/openapi/v3/<path>`.
//!
//! The `?hash=` value is this build's own content hash
//! (`build/openapi_serve.rs`'s doc comment explains why it doesn't need to
//! match whatever algorithm real upstream's apiserver happens to use
//! internally) — a client only ever compares it against a value this same
//! server previously handed back, to decide whether to re-fetch.

use crate::codegen;
use serde_json::{json, Value};

/// `/openapi/v3` — the root discovery index: every servable path, each
/// with a `serverRelativeURL` a client follows to fetch that document.
pub fn root() -> Value {
    let mut paths = serde_json::Map::new();
    for doc in codegen::openapi_v3_docs::OPENAPI_V3_DOCS {
        paths.insert(doc.path.to_string(), json!({"serverRelativeURL": format!("/openapi/v3/{}?hash={}", doc.path, doc.hash)}));
    }
    json!({"paths": paths})
}

/// `/openapi/v3/<path>` — the raw, verbatim vendored document for that
/// path (any `?hash=` query parameter a client sends is accepted but not
/// interpreted; this build always serves its current, single vendored
/// copy of each document rather than a historical version by hash). `None`
/// if this build vendors no such path — a real `404`, not an empty body.
pub fn doc(path: &str) -> Option<&'static [u8]> {
    codegen::openapi_v3_doc_index().get(path).map(|d| d.content)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_lists_a_real_vendored_group_version_with_its_serve_url() {
        let r = root();
        let paths = r["paths"].as_object().unwrap();
        let entry = paths.get("apis/apps/v1").expect("apis/apps/v1 should be a servable OpenAPI v3 path");
        let url = entry["serverRelativeURL"].as_str().unwrap();
        assert!(url.starts_with("/openapi/v3/apis/apps/v1?hash="), "got {url:?}");
    }

    #[test]
    fn root_includes_the_core_v1_groupless_path() {
        let r = root();
        let paths = r["paths"].as_object().unwrap();
        assert!(paths.contains_key("api/v1"), "the groupless core group's own v1 doc should be listed");
    }

    #[test]
    fn doc_serves_real_vendored_json_bytes() {
        let bytes = doc("apis/apps/v1").expect("apis/apps/v1 should be servable");
        let parsed: Value = serde_json::from_slice(bytes).expect("served bytes must be valid JSON");
        // A real OpenAPI v3 document has a top-level "openapi" version
        // field and "components" — cheap structural proof this is the
        // genuine vendored spec, not some placeholder.
        assert!(parsed.get("openapi").is_some());
        assert!(parsed.get("components").is_some());
    }

    #[test]
    fn doc_is_none_for_an_unvendored_path() {
        assert!(doc("apis/totally.made.up/v1").is_none());
    }

    #[test]
    fn every_root_entry_resolves_via_doc() {
        // The two functions must agree with each other: everything root()
        // advertises must actually be fetchable through doc(), and vice
        // versa (same underlying table).
        let r = root();
        for path in r["paths"].as_object().unwrap().keys() {
            assert!(doc(path).is_some(), "{path} is listed in root() but doc() returns None for it");
        }
    }
}
