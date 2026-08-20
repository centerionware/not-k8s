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

/// Resolves a field's `proto_type` (as `proto_fields::ProtoField` stores
/// it — either bare, meaning "same package as the declaring message", or a
/// fully proto-package-qualified name) into the openapi-style qualified
/// name `PROTO_FIELDS`/`FIELD_META` key on, so the protobuf codec (Group
/// B) can look up a referenced message's own field table.
///
/// Only meaningful for a message-typed field — callers must check
/// `is_scalar`/`ProtoField::map` first, since a scalar or `map<K, V>`
/// `proto_type` isn't a message reference this can resolve.
///
/// See `build/proto_parse.rs`'s own module doc for the reverse-DNS bridge
/// this mirrors at runtime instead of re-deriving from the vendored
/// source: a proto-style package's first two dot-segments reversed
/// (`k8s.io` -> `io.k8s`) is the openapi-style form of the same package.
pub fn resolve_message_ref(declaring_message: &str, proto_type: &str) -> String {
    match proto_type.rfind('.') {
        Some(idx) => {
            // Fully proto-package-qualified already (e.g.
            // "k8s.io.apimachinery.pkg.apis.meta.v1.LabelSelector") — swap
            // the package portion into openapi-style form.
            let (pkg, name) = (&proto_type[..idx], &proto_type[idx + 1..]);
            format!("{}.{name}", swap_first_two_segments(pkg))
        }
        None => {
            // Bare — same package as the declaring message, which is
            // already openapi-style, so no swap needed: just borrow its
            // package prefix.
            let pkg = declaring_message.rsplit_once('.').map(|(p, _)| p).unwrap_or("");
            format!("{pkg}.{proto_type}")
        }
    }
}

fn swap_first_two_segments(s: &str) -> String {
    let mut parts: Vec<&str> = s.split('.').collect();
    if parts.len() >= 2 {
        parts.swap(0, 1);
    }
    parts.join(".")
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

    /// Real cases from `DaemonSetSpec`, verified against the vendored
    /// `apps/v1/generated.proto` directly (see the build/proto_parse.rs
    /// module doc): a bare same-package reference and a fully
    /// proto-package-qualified cross-package reference.
    #[test]
    fn resolve_message_ref_handles_bare_and_qualified_references() {
        let daemon_set_spec = "io.k8s.api.apps.v1.DaemonSetSpec";

        // `optional DaemonSetUpdateStrategy updateStrategy = 3;` — bare,
        // same package as DaemonSetSpec itself.
        assert_eq!(
            resolve_message_ref(daemon_set_spec, "DaemonSetUpdateStrategy"),
            "io.k8s.api.apps.v1.DaemonSetUpdateStrategy"
        );

        // `optional .k8s.io.api.core.v1.PodTemplateSpec template = 2;` —
        // fully qualified, a different package.
        assert_eq!(
            resolve_message_ref(daemon_set_spec, "k8s.io.api.core.v1.PodTemplateSpec"),
            "io.k8s.api.core.v1.PodTemplateSpec"
        );

        // Both resolved names must actually exist in the parsed table —
        // resolving to a name nothing defines would be worse than an
        // error, since it would look like it worked.
        assert!(proto_fields::PROTO_MESSAGES.contains(&"io.k8s.api.apps.v1.DaemonSetUpdateStrategy"));
        assert!(proto_fields::PROTO_MESSAGES.contains(&"io.k8s.api.core.v1.PodTemplateSpec"));
    }

    /// The protobuf codec's scalar-type table only handles the scalar
    /// keywords actually observed in the vendored `.proto` set (bool,
    /// bytes, double, int32, int64, string — see `codec::protobuf`'s own
    /// module doc). If a future k8s release introduces a field using
    /// uint32/uint64/sint32/sint64/fixed32/fixed64/float/an enum type, this
    /// fails loudly here instead of the codec silently mis-encoding it.
    #[test]
    fn no_field_uses_a_scalar_type_the_codec_does_not_yet_handle() {
        const KNOWN: &[&str] = &["bool", "bytes", "double", "int32", "int64", "string"];
        for f in proto_fields::PROTO_FIELDS {
            if f.map || f.proto_type.starts_with("map<") {
                continue;
            }
            let is_message_ref = f.proto_type.chars().next().is_some_and(|c| c.is_uppercase())
                || f.proto_type.contains('.');
            if is_message_ref {
                continue;
            }
            assert!(
                KNOWN.contains(&f.proto_type),
                "field {}.{} has scalar type {:?}, not handled by codec::protobuf's wire_type_for()",
                f.message,
                f.json_name,
                f.proto_type
            );
        }
    }
}
