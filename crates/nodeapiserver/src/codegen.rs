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

pub mod api_resources {
    include!(concat!(env!("OUT_DIR"), "/api_resources.rs"));
}

pub mod openapi_v3_docs {
    include!(concat!(env!("OUT_DIR"), "/openapi_v3_docs.rs"));
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

/// `schema -> Vec<&FieldMeta>`, built once from `openapi_meta::FIELD_META` —
/// the "every field this schema carries metadata for" view, as opposed to
/// `field_meta_index()`'s single-field lookup. `scheme::validation`'s
/// recursion (walking every `ref_schema`-bearing field of a schema) is
/// this index's intended reader.
pub fn field_meta_index_by_schema() -> &'static HashMap<&'static str, Vec<&'static openapi_meta::FieldMeta>> {
    static INDEX: OnceLock<HashMap<&'static str, Vec<&'static openapi_meta::FieldMeta>>> = OnceLock::new();
    INDEX.get_or_init(|| {
        let mut m: HashMap<&'static str, Vec<&'static openapi_meta::FieldMeta>> = HashMap::new();
        for f in openapi_meta::FIELD_META {
            m.entry(f.schema).or_default().push(f);
        }
        m
    })
}

/// `schema -> Vec<field name>`, built once from
/// `openapi_meta::REQUIRED_FIELDS`. `scheme::validation` is this index's
/// intended reader.
pub fn required_fields_index() -> &'static HashMap<&'static str, Vec<&'static str>> {
    static INDEX: OnceLock<HashMap<&'static str, Vec<&'static str>>> = OnceLock::new();
    INDEX.get_or_init(|| {
        let mut m: HashMap<&'static str, Vec<&'static str>> = HashMap::new();
        for r in openapi_meta::REQUIRED_FIELDS {
            m.entry(r.schema).or_default().push(r.field);
        }
        m
    })
}

/// `(schema, field) -> openapi_type` ("string"/"integer"/"boolean"/
/// "number"/"array"), built once from `openapi_meta::TYPE_INFO`.
/// `scheme::validation` is this index's intended reader.
pub fn type_info_index() -> &'static HashMap<(&'static str, &'static str), &'static str> {
    static INDEX: OnceLock<HashMap<(&'static str, &'static str), &'static str>> = OnceLock::new();
    INDEX.get_or_init(|| openapi_meta::TYPE_INFO.iter().map(|t| ((t.schema, t.field), t.openapi_type)).collect())
}

/// `(group, version) -> Vec<&ApiResource>`, built once from
/// `api_resources::API_RESOURCES`. `server::discovery`'s per-version
/// `APIResourceList` builder is this index's intended reader.
pub fn api_resources_by_group_version() -> &'static HashMap<(&'static str, &'static str), Vec<&'static api_resources::ApiResource>> {
    static INDEX: OnceLock<HashMap<(&'static str, &'static str), Vec<&'static api_resources::ApiResource>>> = OnceLock::new();
    INDEX.get_or_init(|| {
        let mut m: HashMap<(&'static str, &'static str), Vec<&'static api_resources::ApiResource>> = HashMap::new();
        for r in api_resources::API_RESOURCES {
            m.entry((r.group, r.version)).or_default().push(r);
        }
        m
    })
}

/// `path -> &ServedOpenApiDoc`, built once from
/// `openapi_v3_docs::OPENAPI_V3_DOCS`. `server::openapi`'s `/openapi/v3`
/// endpoints are this index's intended reader.
pub fn openapi_v3_doc_index() -> &'static HashMap<&'static str, &'static openapi_v3_docs::ServedOpenApiDoc> {
    static INDEX: OnceLock<HashMap<&'static str, &'static openapi_v3_docs::ServedOpenApiDoc>> = OnceLock::new();
    INDEX.get_or_init(|| openapi_v3_docs::OPENAPI_V3_DOCS.iter().map(|d| (d.path, d)).collect())
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
        assert_eq!(
            meta.ref_schema.as_deref(),
            Some("io.k8s.api.core.v1.Container"),
            "an array field's ref_schema must resolve to its *element* schema (items.allOf[0].$ref), not the array itself"
        );
    }

    #[test]
    fn every_proto_message_appears_at_least_once() {
        assert!(proto_fields::PROTO_MESSAGES.len() > 100, "expected hundreds of parsed messages");
    }

    /// A single (non-array) object-typed field's ref_schema — the
    /// `allOf: [{$ref: ...}]` shape, distinct from an array field's
    /// `items.allOf[0].$ref` (covered by `pod_schema_has_a_gvk_and_fields`
    /// above).
    #[test]
    fn a_non_array_object_field_resolves_its_own_ref_schema() {
        let meta = field_meta_index()
            .get(&("io.k8s.api.apps.v1.DaemonSetSpec", "selector"))
            .expect("DaemonSetSpec.selector should carry ref_schema metadata");
        assert_eq!(meta.ref_schema.as_deref(), Some("io.k8s.apimachinery.pkg.apis.meta.v1.LabelSelector"));
    }

    /// A plain scalar field with no default and no other x-kubernetes-*
    /// metadata isn't in the table at all.
    #[test]
    fn a_scalar_field_with_no_default_has_no_field_meta_entry() {
        assert!(field_meta_index().get(&("io.k8s.api.apps.v1.DaemonSetSpec", "minReadySeconds")).is_none());
    }

    /// A real, meaningful scalar default — `ContainerPort.protocol`
    /// defaults to `"TCP"` — captured even though the field has no other
    /// x-kubernetes-* metadata at all.
    #[test]
    fn a_scalar_fields_real_default_value_is_captured() {
        let meta = field_meta_index()
            .get(&("io.k8s.api.core.v1.ContainerPort", "protocol"))
            .expect("ContainerPort.protocol should carry a default");
        assert_eq!(meta.default_json, Some("\"TCP\""));
    }

    /// Real, verified vendored `required` arrays: `ContainerPort` requires
    /// only `containerPort`; `Container` requires only `name` (not
    /// `image` — a real, easy-to-assume-wrong fact about the schema).
    #[test]
    fn required_fields_index_reflects_the_real_vendored_required_arrays() {
        let idx = required_fields_index();
        assert_eq!(idx.get("io.k8s.api.core.v1.ContainerPort").map(Vec::as_slice), Some(&["containerPort"][..]));
        assert_eq!(idx.get("io.k8s.api.core.v1.Container").map(Vec::as_slice), Some(&["name"][..]));
    }

    /// `ObjectMeta` has no `required` array at all in the vendored spec —
    /// every one of its fields is optional at the structural level.
    #[test]
    fn a_schema_with_no_required_array_has_no_index_entry() {
        assert!(required_fields_index().get("io.k8s.apimachinery.pkg.apis.meta.v1.ObjectMeta").is_none());
    }

    /// Real, verified vendored `type` declarations from `PodSpec`: an
    /// array field (`containers`), a scalar boolean (`hostNetwork`), and a
    /// nested-object field (`securityContext`, spelled via `allOf` with no
    /// sibling `"type"` key) which correctly has *no* entry — that shape
    /// is `ref_schema`'s job, not this table's.
    #[test]
    fn type_info_index_reflects_real_vendored_type_declarations() {
        let idx = type_info_index();
        assert_eq!(idx.get(&("io.k8s.api.core.v1.PodSpec", "containers")), Some(&"array"));
        assert_eq!(idx.get(&("io.k8s.api.core.v1.PodSpec", "hostNetwork")), Some(&"boolean"));
        assert_eq!(idx.get(&("io.k8s.api.core.v1.PodSpec", "securityContext")), None);
    }

    /// Real, verified per-version discovery facts: `pods` is namespaced
    /// with a full CRUD+watch verb set (merged across the namespaced list
    /// path, the `/api/v1/pods` list-all-namespaces path, and the
    /// single-item path); `nodes` is genuinely cluster-scoped; `namespaces`
    /// resolves to the `Namespace` object itself (the `{namespace}` vs
    /// `{name}` disambiguation this parser exists for) rather than being
    /// mistaken for some namespaced resource named "namespaces".
    #[test]
    fn api_resources_reflects_real_verbs_and_namespaced_ness() {
        let core_v1 = api_resources_by_group_version().get(&("", "v1")).expect("core/v1 should have discovered resources");
        let pods = core_v1.iter().find(|r| r.resource == "pods").expect("pods should be discovered");
        assert!(pods.namespaced);
        assert_eq!(pods.kind, "Pod");
        for verb in ["get", "list", "create", "update", "patch", "delete", "deletecollection", "watch"] {
            assert!(pods.verbs.contains(&verb), "pods should support verb {verb:?}, got {:?}", pods.verbs);
        }

        let nodes = core_v1.iter().find(|r| r.resource == "nodes").expect("nodes should be discovered");
        assert!(!nodes.namespaced, "Node is cluster-scoped");

        let namespaces = core_v1.iter().find(|r| r.resource == "namespaces").expect("namespaces should be discovered");
        assert_eq!(namespaces.kind, "Namespace");
        assert!(!namespaces.namespaced, "Namespace itself is cluster-scoped, not namespaced");
    }

    /// A subresource path (`pods/{name}/status`) must not produce its own
    /// bogus top-level resource entry (e.g. a "status" resource) — it's a
    /// named, deliberate skip (see `build/discovery_parse.rs`'s own doc),
    /// not something this parser should silently misinterpret.
    #[test]
    fn a_subresource_path_never_produces_a_spurious_top_level_resource() {
        let core_v1 = api_resources_by_group_version().get(&("", "v1")).expect("core/v1 should have discovered resources");
        assert!(core_v1.iter().all(|r| r.resource != "status"));
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
