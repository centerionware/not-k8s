//! Per-version resource discovery (`APIResourceList`): parses the vendored
//! OpenAPI v3 specs' own `paths` section — the gap `server::discovery`'s
//! own module doc names explicitly ("Group A's codegen would need to also
//! parse the OpenAPI spec's paths section... real, separate work").
//!
//! Each REST path carries `x-kubernetes-action` (the verb this specific
//! HTTP method performs — `get`/`list`/`post`/`put`/`patch`/`delete`/
//! `deletecollection`/`watch`/`watchlist`/`connect`) and
//! `x-kubernetes-group-version-kind` on every verb block. Grouping by
//! `(group, version, resource)` and unioning every path's verbs and
//! namespaced-ness across the whole spec produces exactly the table real
//! kube-apiserver discovery serves per resource.
//!
//! # What this captures, and what it deliberately skips (named, not silent)
//!
//! - The deprecated `/api/v1/watch/...` / `/apis/{group}/{version}/watch/...`
//!   path family (the pre-1.0-style watch API, superseded by `?watch=true`
//!   on the plain list path — its own `x-kubernetes-action` values are
//!   `watch`/`watchlist` but attached to a *different, legacy* route this
//!   build doesn't need to discover separately) — skipped outright. This
//!   means "watch" never appears as a literal `x-kubernetes-action` on any
//!   path this parser actually reads (the modern GET-collection route's
//!   own action is `list`) — `parse_all` synthesizes a `"watch"` verb
//!   whenever `"list"` is present instead, since every real REST storage
//!   supporting list also supports watching that same route.
//! - Subresources (`.../pods/{name}/status`, `/log`, `/exec`, `/proxy`,
//!   ...) — included as their own `APIResource` entries with names such as
//!   `pods/status`, matching real discovery. Paths with an additional
//!   wildcard tail (for example `pods/{name}/proxy/{path}`) remain skipped.
//! - `connect` actions (proxy/exec/attach/portforward) — retained on the
//!   corresponding subresource entry. They are never added to the parent
//!   resource because the resource key includes the subresource suffix.
//!
//! # The `{namespace}` vs `{name}` disambiguation
//!
//! `/api/v1/namespaces` (list Namespaces) and
//! `/api/v1/namespaces/{namespace}/pods` (list Pods in a namespace) both
//! start with the literal segment `namespaces` — telling them apart isn't
//! optional. Confirmed directly against the vendored spec: the `Namespace`
//! object's own single-item path is `/api/v1/namespaces/{name}` (parameter
//! named `name`), while every namespaced-resource path uses
//! `/namespaces/{namespace}/...` (parameter named `namespace`) — a
//! different placeholder name, not the same one reused. This parser keys
//! off exactly that distinction rather than hard-coding "namespaces" as a
//! special case.

use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

#[derive(Debug, Clone)]
pub struct ResourceEntry {
    pub group: String,
    pub version: String,
    pub resource: String,
    pub kind: String,
    pub namespaced: bool,
    pub verbs: Vec<String>,
}

pub fn parse_all(root: &Path) -> Vec<ResourceEntry> {
    // (group, version, resource) -> (kind, namespaced-any, verbs)
    let mut acc: BTreeMap<(String, String, String), (String, bool, BTreeSet<String>)> = BTreeMap::new();

    let mut files: Vec<_> = std::fs::read_dir(root)
        .unwrap_or_else(|e| panic!("reading {}: {e}", root.display()))
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("json"))
        .collect();
    files.sort();

    for path in &files {
        let raw = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("reading {}: {e}", path.display()));
        let doc: Value = serde_json::from_str(&raw).unwrap_or_else(|e| panic!("parsing {}: {e}", path.display()));
        let Some(paths) = doc.get("paths").and_then(Value::as_object) else { continue };

        for (path_str, path_obj) in paths {
            let Some((namespaced, resource_segs)) = classify_path(path_str) else { continue };
            // Collection, single-item, and single-item-subresource paths
            // contribute verbs to the same resource entry.
            let Some(resource) = resource_shape(resource_segs) else { continue };

            let Some(verb_blocks) = path_obj.as_object() else { continue };
            for (_, verb_obj) in verb_blocks {
                let Some(action) = verb_obj.get("x-kubernetes-action").and_then(Value::as_str) else { continue };
                let Some(verb) = normalize_verb(action) else { continue };
                let Some(gvk) = verb_obj.get("x-kubernetes-group-version-kind") else { continue };
                let (Some(group), Some(version), Some(kind)) = (
                    gvk.get("group").and_then(Value::as_str),
                    gvk.get("version").and_then(Value::as_str),
                    gvk.get("kind").and_then(Value::as_str),
                ) else {
                    continue;
                };

                let entry = acc
                    .entry((group.to_string(), version.to_string(), resource.to_string()))
                    .or_insert_with(|| (kind.to_string(), false, BTreeSet::new()));
                entry.1 |= namespaced;
                entry.2.insert(verb.to_string());
            }
        }
    }

    acc.into_iter()
        .map(|((group, version, resource), (kind, namespaced, mut verbs))| {
            // "watch" never appears on the modern GET-collection route's
            // own x-kubernetes-action (confirmed against the vendored
            // spec directly: that route's action is "list"; "watch" only
            // ever labels the deprecated `/watch/`-prefixed legacy route
            // family, which this parser skips outright per its own doc
            // comment). Every real REST storage that supports `list` also
            // supports watching the same collection via `?watch=true` on
            // that exact route — a universal pairing in kube-apiserver's
            // own generic registry (`rest.Lister`/`rest.Watcher`), not a
            // per-type exception — so synthesize the "watch" verb from
            // "list" rather than leaving every resource's own discovery
            // silently missing a verb it genuinely supports.
            if verbs.contains("list") {
                verbs.insert("watch".to_string());
            }
            ResourceEntry { group, version, resource, kind, namespaced, verbs: verbs.into_iter().collect() }
        })
        .collect()
}

/// Splits a path into `(namespaced, remaining_segments)`, or `None` to
/// skip it entirely (root document paths, the deprecated `/watch/` family,
/// or anything not under `/api`/`/apis`).
fn classify_path(path_str: &str) -> Option<(bool, Vec<&str>)> {
    let segs: Vec<&str> = path_str.trim_matches('/').split('/').filter(|s| !s.is_empty()).collect();
    let rest: &[&str] = match segs.first().copied() {
        Some("api") => &segs[2.min(segs.len())..], // "api", version
        Some("apis") => &segs[3.min(segs.len())..], // "apis", group, version
        _ => return None,
    };
    if rest.is_empty() || rest[0] == "watch" {
        return None;
    }
    if rest[0] == "namespaces" && rest.get(1) == Some(&"{namespace}") {
        Some((true, rest[2..].to_vec()))
    } else {
        Some((false, rest.to_vec()))
    }
}

/// `segs` is what's left after stripping any namespace prefix: either
/// `["pods"]`/`["pods", "{name}"]` (this slice's two known shapes) or a
/// subresource's longer tail, which returns `None` — named skip, not a
/// silent drop, per this module's own doc comment.
fn resource_shape(segs: Vec<&str>) -> Option<String> {
    match segs.as_slice() {
        [resource] if !resource.starts_with('{') => Some(resource.to_string()),
        [resource, name] if !resource.starts_with('{') && *name == "{name}" => Some(resource.to_string()),
        [resource, name, subresource]
            if !resource.starts_with('{')
                && *name == "{name}"
                && !subresource.starts_with('{') =>
        {
            Some(format!("{resource}/{subresource}"))
        }
        _ => None,
    }
}

fn normalize_verb(action: &str) -> Option<&'static str> {
    match action {
        "get" => Some("get"),
        "list" => Some("list"),
        "post" => Some("create"),
        "put" => Some("update"),
        "patch" => Some("patch"),
        "delete" => Some("delete"),
        "deletecollection" => Some("deletecollection"),
        "watch" => Some("watch"),
        "connect" => Some("connect"),
        // "watchlist" only ever appears on the already-skipped deprecated
        // /watch/ path family. Fail closed rather than guessing at an
        // unrecognized action.
        _ => None,
    }
}

pub fn render(entries: &[ResourceEntry]) -> String {
    let mut sorted: Vec<&ResourceEntry> = entries.iter().collect();
    sorted.sort_by(|a, b| (a.group.as_str(), a.version.as_str(), a.resource.as_str()).cmp(&(b.group.as_str(), b.version.as_str(), b.resource.as_str())));

    let mut out = String::new();
    out.push_str("pub struct ApiResource {\n");
    out.push_str("    pub group: &'static str,\n");
    out.push_str("    pub version: &'static str,\n");
    out.push_str("    pub resource: &'static str,\n");
    out.push_str("    pub kind: &'static str,\n");
    out.push_str("    pub namespaced: bool,\n");
    out.push_str("    pub verbs: &'static [&'static str],\n");
    out.push_str("}\n\n");

    out.push_str("pub static API_RESOURCES: &[ApiResource] = &[\n");
    for e in &sorted {
        let verbs: Vec<String> = e.verbs.iter().map(|v| format!("{v:?}")).collect();
        out.push_str(&format!(
            "    ApiResource {{ group: {:?}, version: {:?}, resource: {:?}, kind: {:?}, namespaced: {}, verbs: &[{}] }},\n",
            e.group,
            e.version,
            e.resource,
            e.kind,
            e.namespaced,
            verbs.join(", "),
        ));
    }
    out.push_str("];\n");
    out
}
