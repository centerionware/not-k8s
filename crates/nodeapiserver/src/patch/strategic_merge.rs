//! Strategic Merge Patch — the k8s-specific patch semantics no crate
//! implements (`docs/APISERVER_PLAN.md` finding 8), driven by Group A's
//! `FIELD_META` table (now carrying `ref_schema`, added alongside this
//! module specifically so recursion has real per-field metadata to read
//! instead of inheriting the parent's).
//!
//! # Semantics implemented
//!
//! - A `null` value in the patch deletes that key from the result — same
//!   convention JSON Merge Patch uses, and the one real kube-apiserver's
//!   own SMP follows too.
//! - An object-typed field merges recursively (the JSON-merge default;
//!   no `x-kubernetes-*` annotation is needed for this — only *list*
//!   fields need one to know how to merge).
//! - A list field whose `FIELD_META` entry has
//!   `patch_strategy == Some("merge")` merges by `patch_merge_key`: a
//!   patch element whose key value matches an existing element merges
//!   into it (recursively, using the list's own `ref_schema` — the
//!   *element* schema); one that doesn't match is appended. Original
//!   order is preserved; new elements land at the end.
//! - Every other field (scalars, and list fields without `merge`
//!   strategy) is replaced wholesale by the patch's value — matches
//!   JSON Merge Patch's own default for exactly the same reason: no
//!   metadata says how to do anything smarter.
//!
//! # Named, deliberate simplifications
//!
//! - No `$patch: "delete"` / `$patch: "replace"` directives, no
//!   `$deleteFromPrimitiveList`/`$setElementOrder` — upstream's own
//!   advanced directive set for explicit list-item deletion and ordering
//!   control. A patch that never uses them (the overwhelming majority of
//!   real `kubectl apply`/controller patches) behaves identically either
//!   way; one that does use them is not yet honored.
//! - A merge-list element that isn't a JSON object (patch-merge-key only
//!   ever applies to object elements upstream too) falls back to
//!   wholesale list replacement rather than attempting to merge scalars
//!   by value.
//! - `map_type`/`x-kubernetes-map-type` (SSA's map-vs-granular distinction)
//!   isn't consulted — SMP itself doesn't need it; Server-Side Apply
//!   (unimplemented, see `patch/mod.rs`) will.

use crate::codegen;
use serde_json::Value;

/// Applies a strategic merge patch to `original`, which is understood to
/// be shaped like `schema` (an openapi-style qualified schema name, e.g.
/// `"io.k8s.api.core.v1.PodSpec"` — the same key `FIELD_META`/`DISCOVERY_GVKS`
/// use). Returns the merged result; never mutates `original` in place,
/// matching every other patch function in this module (`json_patch`,
/// `merge_patch`) taking `&mut Value` instead — SMP is kept
/// value-in/value-out since almost every call site here needs the
/// resolved schema threaded through the recursion anyway, so returning a
/// fresh `Value` costs nothing extra.
pub fn apply(schema: &str, original: &Value, patch: &Value) -> Value {
    merge(schema, original, patch)
}

fn merge(schema: &str, original: &Value, patch: &Value) -> Value {
    let (Value::Object(orig_map), Value::Object(patch_map)) = (original, patch) else {
        // Types don't align to merge (patch replaces a scalar with an
        // object, or vice versa, or either side already isn't an object)
        // — the patch's value wins wholesale, same as JSON Merge Patch's
        // own fallback.
        return patch.clone();
    };

    let mut result = orig_map.clone();
    for (key, patch_value) in patch_map {
        if patch_value.is_null() {
            result.remove(key);
            continue;
        }

        let meta = codegen::field_meta_index().get(&(schema, key.as_str()));
        let existing = result.get(key).cloned();

        let merged = match (existing, patch_value) {
            (Some(Value::Array(orig_list)), Value::Array(patch_list))
                if meta.is_some_and(|m| m.patch_strategy == Some("merge")) =>
            {
                let m = meta.expect("checked by is_some_and above");
                Value::Array(merge_list(m.ref_schema, m.patch_merge_key, &orig_list, patch_list))
            }
            (Some(orig_value @ Value::Object(_)), Value::Object(_)) => {
                // Recurse using *this field's own* ref_schema when known;
                // falling back to the parent's schema would be wrong the
                // moment the nested object is a different type (the whole
                // reason ref_schema was added), so an unknown ref_schema
                // instead merges generically: recurse structurally without
                // consulting FIELD_META at all (equivalent to every nested
                // key being treated as a plain scalar/object merge, no
                // list-merge behavior available at that level). Correct
                // for plain nested objects most of the time; a nested
                // *list* field one level down whose own schema we don't
                // know will replace wholesale rather than merge-by-key —
                // named here rather than silently wrong.
                merge(meta.and_then(|m| m.ref_schema).unwrap_or(""), &orig_value, patch_value)
            }
            _ => patch_value.clone(),
        };
        result.insert(key.clone(), merged);
    }
    Value::Object(result)
}

/// Merges a `patch_strategy: merge` list: patch elements matching an
/// existing element's `merge_key` value merge into it (recursively);
/// non-matching ones append. `merge_key: None` (a `merge` list somehow
/// missing its key — malformed metadata, not a real case in the vendored
/// data) or a non-object element falls back to wholesale replacement,
/// since there is no identity to merge by.
fn merge_list(element_schema: Option<&str>, merge_key: Option<&str>, original: &[Value], patch: &[Value]) -> Vec<Value> {
    let Some(key) = merge_key else {
        return patch.to_vec();
    };
    if patch.iter().any(|e| !e.is_object()) || original.iter().any(|e| !e.is_object()) {
        return patch.to_vec();
    }

    let mut result = original.to_vec();
    'patch_elements: for patch_elem in patch {
        let Some(patch_key_value) = patch_elem.get(key) else {
            // A merge-list element missing its own merge key can't be
            // matched against anything — append it as-is, same as
            // upstream treats an unidentifiable element.
            result.push(patch_elem.clone());
            continue;
        };
        for existing in result.iter_mut() {
            if existing.get(key) == Some(patch_key_value) {
                *existing = merge(element_schema.unwrap_or(""), existing, patch_elem);
                continue 'patch_elements;
            }
        }
        result.push(patch_elem.clone());
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_scalar_field_is_replaced() {
        let original = json!({"minReadySeconds": 5});
        let patch = json!({"minReadySeconds": 10});
        let merged = apply("io.k8s.api.apps.v1.DaemonSetSpec", &original, &patch);
        assert_eq!(merged, json!({"minReadySeconds": 10}));
    }

    #[test]
    fn a_null_value_deletes_the_field() {
        let original = json!({"minReadySeconds": 5, "revisionHistoryLimit": 10});
        let patch = json!({"minReadySeconds": null});
        let merged = apply("io.k8s.api.apps.v1.DaemonSetSpec", &original, &patch);
        assert_eq!(merged, json!({"revisionHistoryLimit": 10}));
    }

    #[test]
    fn an_object_field_merges_recursively_using_its_own_ref_schema() {
        // DaemonSetSpec.selector -> LabelSelector, whose own matchLabels
        // field is map<string,string> (merges key-by-key, the plain
        // object-merge default — no patch_strategy metadata needed for a
        // map either).
        let original = json!({"selector": {"matchLabels": {"app": "web", "tier": "frontend"}}});
        let patch = json!({"selector": {"matchLabels": {"tier": "backend"}}});
        let merged = apply("io.k8s.api.apps.v1.DaemonSetSpec", &original, &patch);
        assert_eq!(merged, json!({"selector": {"matchLabels": {"app": "web", "tier": "backend"}}}));
    }

    /// The concrete sample `docs/APISERVER_PLAN.md` finding 5 names
    /// directly: `PodSpec.containers` is `list-type: map`,
    /// `patch-merge-key: name`, `patch-strategy: merge`.
    #[test]
    fn a_merge_list_field_merges_matching_elements_by_key_and_appends_the_rest() {
        let original = json!({
            "containers": [
                {"name": "app", "image": "app:v1"},
                {"name": "sidecar", "image": "sidecar:v1"},
            ]
        });
        let patch = json!({
            "containers": [
                {"name": "app", "image": "app:v2"},
                {"name": "new-one", "image": "new:v1"},
            ]
        });
        let merged = apply("io.k8s.api.core.v1.PodSpec", &original, &patch);
        let containers = merged["containers"].as_array().unwrap();
        assert_eq!(containers.len(), 3, "one updated in place, one untouched, one appended");
        assert_eq!(containers[0], json!({"name": "app", "image": "app:v2"}), "matched element merges in place, preserving position");
        assert_eq!(containers[1], json!({"name": "sidecar", "image": "sidecar:v1"}), "unmatched original element is untouched");
        assert_eq!(containers[2], json!({"name": "new-one", "image": "new:v1"}), "unmatched patch element appends");
    }

    #[test]
    fn a_non_merge_list_field_is_replaced_wholesale() {
        // Container.args has no patch_strategy — a plain array field.
        let original = json!({"args": ["--old"]});
        let patch = json!({"args": ["--new1", "--new2"]});
        let merged = apply("io.k8s.api.core.v1.Container", &original, &patch);
        assert_eq!(merged, json!({"args": ["--new1", "--new2"]}));
    }

    #[test]
    fn fields_absent_from_the_patch_are_left_untouched() {
        let original = json!({"minReadySeconds": 5, "revisionHistoryLimit": 10});
        let patch = json!({"minReadySeconds": 6});
        let merged = apply("io.k8s.api.apps.v1.DaemonSetSpec", &original, &patch);
        assert_eq!(merged["revisionHistoryLimit"], json!(10));
    }

    #[test]
    fn merging_a_nested_container_field_recurses_two_levels_deep() {
        // containers[].resources is itself an object field on Container —
        // proves ref_schema resolution chains correctly across more than
        // one level of recursion, not just parent -> immediate child.
        let original = json!({
            "containers": [
                {"name": "app", "resources": {"limits": {"cpu": "1"}}},
            ]
        });
        let patch = json!({
            "containers": [
                {"name": "app", "resources": {"limits": {"memory": "512Mi"}}},
            ]
        });
        let merged = apply("io.k8s.api.core.v1.PodSpec", &original, &patch);
        assert_eq!(merged["containers"][0]["resources"]["limits"], json!({"cpu": "1", "memory": "512Mi"}));
    }
}
