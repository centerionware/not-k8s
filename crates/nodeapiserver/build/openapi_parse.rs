//! Parses the vendored OpenAPI v3 specs (`vendor/openapi-spec/v3/*.json`)
//! into two tables `build.rs` emits as Rust source:
//!
//! - `FIELD_META` — the `x-kubernetes-*` extensions each schema property
//!   carries (`docs/APISERVER_PLAN.md` finding 5): list-type/list-map-keys
//!   for Server-Side Apply, patch-strategy/patch-merge-key for Strategic
//!   Merge Patch, map-type for SSA map semantics.
//! - `DISCOVERY_GVKS` — the `x-kubernetes-group-version-kind` extension on
//!   each top-level object schema, i.e. the discovery GVK <-> schema-name
//!   map every REST-endpoint installer and the `/openapi/v3` response
//!   itself need.
//!
//! One vendored artifact set serves both, per finding 5 — nothing here
//! hand-maintains a list of which type has which patch strategy.

use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

#[derive(Debug, Clone, Default)]
pub struct FieldMeta {
    pub schema: String,
    pub field: String,
    pub list_type: Option<String>,
    pub list_map_keys: Vec<String>,
    pub map_type: Option<String>,
    pub patch_strategy: Option<String>,
    pub patch_merge_key: Option<String>,
}

#[derive(Debug, Clone)]
pub struct GvkEntry {
    pub schema: String,
    pub group: String,
    pub version: String,
    pub kind: String,
}

/// A shared type like `ObjectMeta` is fully embedded (not just `$ref`'d) in
/// nearly every one of the 64 vendored files — each group-version spec is
/// self-contained. Naive per-file collection would therefore duplicate its
/// entries ~64x with identical content. Dedup by (schema[, field]) as we
/// go — a `BTreeSet`/`BTreeMap` keyed on identity rather than a plain `Vec`
/// — since the extension metadata for a given schema is the same truth
/// regardless of which file it happened to be embedded in.
pub fn parse_all(root: &Path) -> (Vec<FieldMeta>, Vec<GvkEntry>) {
    let mut field_meta: BTreeMap<(String, String), FieldMeta> = BTreeMap::new();
    let mut gvks: BTreeSet<(String, String, String, String)> = BTreeSet::new();
    let mut files: Vec<_> = std::fs::read_dir(root)
        .unwrap_or_else(|e| panic!("reading {}: {e}", root.display()))
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("json"))
        .collect();
    files.sort();

    for path in &files {
        let raw = std::fs::read_to_string(path)
            .unwrap_or_else(|e| panic!("reading {}: {e}", path.display()));
        let doc: Value = serde_json::from_str(&raw)
            .unwrap_or_else(|e| panic!("parsing {}: {e}", path.display()));
        let Some(schemas) = doc.pointer("/components/schemas").and_then(Value::as_object) else {
            continue;
        };
        for (schema_name, schema) in schemas {
            if let Some(gvk_ext) = schema.get("x-kubernetes-group-version-kind") {
                collect_gvks(schema_name, gvk_ext, &mut gvks);
            }
            let Some(props) = schema.get("properties").and_then(Value::as_object) else {
                continue;
            };
            for (field_name, prop) in props {
                if let Some(meta) = extension_meta(schema_name, field_name, prop) {
                    field_meta.insert((schema_name.clone(), field_name.clone()), meta);
                }
            }
        }
    }
    let gvks = gvks
        .into_iter()
        .map(|(schema, group, version, kind)| GvkEntry { schema, group, version, kind })
        .collect();
    (field_meta.into_values().collect(), gvks)
}

fn collect_gvks(schema_name: &str, ext: &Value, out: &mut BTreeSet<(String, String, String, String)>) {
    // Either a single {group,version,kind} object or an array of them
    // (a handful of types, e.g. those shared across internal/external
    // representations, carry more than one GVK).
    let entries: Vec<&Value> = match ext {
        Value::Array(items) => items.iter().collect(),
        Value::Object(_) => vec![ext],
        _ => return,
    };
    for e in entries {
        let (Some(group), Some(version), Some(kind)) = (
            e.get("group").and_then(Value::as_str),
            e.get("version").and_then(Value::as_str),
            e.get("kind").and_then(Value::as_str),
        ) else {
            continue;
        };
        out.insert((schema_name.to_string(), group.to_string(), version.to_string(), kind.to_string()));
    }
}

fn extension_meta(schema_name: &str, field_name: &str, prop: &Value) -> Option<FieldMeta> {
    let list_type = prop.get("x-kubernetes-list-type").and_then(Value::as_str).map(str::to_string);
    let map_type = prop.get("x-kubernetes-map-type").and_then(Value::as_str).map(str::to_string);
    let patch_strategy = prop.get("x-kubernetes-patch-strategy").and_then(Value::as_str).map(str::to_string);
    let patch_merge_key = prop.get("x-kubernetes-patch-merge-key").and_then(Value::as_str).map(str::to_string);
    let list_map_keys: Vec<String> = prop
        .get("x-kubernetes-list-map-keys")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect())
        .unwrap_or_default();

    if list_type.is_none() && map_type.is_none() && patch_strategy.is_none() && patch_merge_key.is_none() && list_map_keys.is_empty() {
        return None;
    }
    Some(FieldMeta {
        schema: schema_name.to_string(),
        field: field_name.to_string(),
        list_type,
        list_map_keys,
        map_type,
        patch_strategy,
        patch_merge_key,
    })
}

fn opt_str(o: &Option<String>) -> String {
    match o {
        Some(s) => format!("Some({s:?})"),
        None => "None".to_string(),
    }
}

pub fn render(field_meta: &[FieldMeta], gvks: &[GvkEntry]) -> String {
    let mut by_schema: BTreeMap<&str, Vec<&FieldMeta>> = BTreeMap::new();
    for m in field_meta {
        by_schema.entry(m.schema.as_str()).or_default().push(m);
    }
    let mut sorted_gvks: Vec<&GvkEntry> = gvks.iter().collect();
    sorted_gvks.sort_by(|a, b| a.schema.cmp(&b.schema));

    let mut out = String::new();
    out.push_str("// @generated by build.rs (openapi_parse) from vendor/openapi-spec/v3 — do not edit.\n\n");

    out.push_str("pub struct FieldMeta {\n");
    out.push_str("    pub schema: &'static str,\n");
    out.push_str("    pub field: &'static str,\n");
    out.push_str("    pub list_type: Option<&'static str>,\n");
    out.push_str("    pub list_map_keys: &'static [&'static str],\n");
    out.push_str("    pub map_type: Option<&'static str>,\n");
    out.push_str("    pub patch_strategy: Option<&'static str>,\n");
    out.push_str("    pub patch_merge_key: Option<&'static str>,\n");
    out.push_str("}\n\n");

    out.push_str("pub static FIELD_META: &[FieldMeta] = &[\n");
    for (schema, group) in &by_schema {
        for m in group {
            let keys: Vec<String> = m.list_map_keys.iter().map(|k| format!("{k:?}")).collect();
            out.push_str(&format!(
                "    FieldMeta {{ schema: {:?}, field: {:?}, list_type: {}, list_map_keys: &[{}], map_type: {}, patch_strategy: {}, patch_merge_key: {} }},\n",
                schema,
                m.field,
                opt_str(&m.list_type),
                keys.join(", "),
                opt_str(&m.map_type),
                opt_str(&m.patch_strategy),
                opt_str(&m.patch_merge_key),
            ));
        }
    }
    out.push_str("];\n\n");

    out.push_str("pub struct GvkEntry {\n");
    out.push_str("    pub schema: &'static str,\n");
    out.push_str("    pub group: &'static str,\n");
    out.push_str("    pub version: &'static str,\n");
    out.push_str("    pub kind: &'static str,\n");
    out.push_str("}\n\n");

    out.push_str("pub static DISCOVERY_GVKS: &[GvkEntry] = &[\n");
    for g in &sorted_gvks {
        out.push_str(&format!(
            "    GvkEntry {{ schema: {:?}, group: {:?}, version: {:?}, kind: {:?} }},\n",
            g.schema, g.group, g.version, g.kind
        ));
    }
    out.push_str("];\n");

    out
}
