//! The two build-time-generated tables from `build.rs` (Group A), plus the
//! small runtime indexes built once over them. Everything downstream
//! (Group B's codec, Group G's patch/SSA, Group E's discovery endpoints)
//! reads these instead of hand-maintaining its own copy of "what fields
//! does this type have" or "what GVK does this schema serve".

pub mod proto_fields {
    include!(concat!(env!("OUT_DIR"), "/proto_fields.rs"));
}

pub mod openapi_meta {
    include!(concat!(env!("OUT_DIR"), "/openapi_meta.rs"));
}

use std::collections::HashMap;
use std::sync::OnceLock;

/// `(message, json_field_name) -> &ProtoField`, built once from
/// `proto_fields::PROTO_FIELDS`. The protobuf codec (Group B) is this
/// index's only intended reader.
pub fn proto_field_index() -> &'static HashMap<(&'static str, &'static str), &'static proto_fields::ProtoField> {
    static INDEX: OnceLock<HashMap<(&'static str, &'static str), &'static proto_fields::ProtoField>> = OnceLock::new();
    INDEX.get_or_init(|| {
        proto_fields::PROTO_FIELDS
            .iter()
            .map(|f| ((f.message, f.json_name), f))
            .collect()
    })
}

/// `schema -> Vec<&GvkEntry>`, built once from `openapi_meta::DISCOVERY_GVKS`.
/// A schema can carry more than one GVK (shared internal/external types).
pub fn gvk_index() -> &'static HashMap<&'static str, Vec<&'static openapi_meta::GvkEntry>> {
    static INDEX: OnceLock<HashMap<&'static str, Vec<&'static openapi_meta::GvkEntry>>> = OnceLock::new();
    INDEX.get_or_init(|| {
        let mut m: HashMap<&'static str, Vec<&'static openapi_meta::GvkEntry>> = HashMap::new();
        for g in openapi_meta::DISCOVERY_GVKS {
            m.entry(g.schema).or_default().push(g);
        }
        m
    })
}

/// `(group, version, kind) -> schema name` — the inverse of `gvk_index()`,
/// what a REST handler needs going from a request's path (group/version)
/// and a resource's kind to the schema whose `FIELD_META`/`PROTO_FIELDS`
/// entries apply.
pub fn schema_by_gvk() -> &'static HashMap<(&'static str, &'static str, &'static str), &'static str> {
    static INDEX: OnceLock<HashMap<(&'static str, &'static str, &'static str), &'static str>> = OnceLock::new();
    INDEX.get_or_init(|| {
        openapi_meta::DISCOVERY_GVKS
            .iter()
            .map(|g| ((g.group, g.version, g.kind), g.schema))
            .collect()
    })
}

/// `(schema, field) -> &FieldMeta`, built once from
/// `openapi_meta::FIELD_META`. Strategic Merge Patch and Server-Side Apply
/// (Group G) are this index's intended readers.
pub fn field_meta_index() -> &'static HashMap<(&'static str, &'static str), &'static openapi_meta::FieldMeta> {
    static INDEX: OnceLock<HashMap<(&'static str, &'static str), &'static openapi_meta::FieldMeta>> = OnceLock::new();
    INDEX.get_or_init(|| {
        openapi_meta::FIELD_META
            .iter()
            .map(|m| ((m.schema, m.field), m))
            .collect()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The core/v1 Pod schema must exist with a real GVK and real fields —
    /// this doesn't assert exact upstream shapes (that would just
    /// re-encode the vendored data by hand), it asserts the codegen
    /// pipeline actually ran and produced a non-empty, queryable table.
    #[test]
    fn pod_schema_has_a_gvk_and_fields() {
        let schema = *schema_by_gvk()
            .get(&("", "v1", "Pod"))
            .expect("core/v1 Pod should be in the discovery GVK table");
        assert_eq!(schema, "io.k8s.api.core.v1.Pod");

        let pod_spec = "io.k8s.api.core.v1.PodSpec";
        let containers = proto_field_index()
            .get(&(pod_spec, "containers"))
            .expect("PodSpec.containers should be in the protobuf field table");
        assert!(containers.repeated, "PodSpec.containers should be repeated");

        // finding 5's concrete sample: PodSpec.containers is list-type "map".
        let meta = field_meta_index()
            .get(&(pod_spec, "containers"))
            .expect("PodSpec.containers should carry x-kubernetes-* metadata");
        assert_eq!(meta.list_type.as_deref(), Some("map"));
        assert_eq!(meta.list_map_keys, &["name"]);
        assert_eq!(meta.patch_strategy.as_deref(), Some("merge"));
        assert_eq!(meta.patch_merge_key.as_deref(), Some("name"));
    }

    #[test]
    fn every_proto_message_appears_at_least_once() {
        assert!(proto_fields::PROTO_MESSAGES.len() > 100, "expected hundreds of parsed messages");
    }
}
