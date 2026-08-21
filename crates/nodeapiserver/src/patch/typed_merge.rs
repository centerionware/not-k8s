//! Server-Side Apply's own real *merge* — `typed.mergingWalker`, ported
//! (`sigs.k8s.io/structured-merge-diff/v6/typed/merge.go`, fetched and
//! read directly). Combines a live object with an incoming apply
//! configuration into the merged result real upstream's own
//! `Updater.Apply` would produce as `liveObject.Merge(configObject)`'s
//! own half of the algorithm — the other half (`Updater.Apply` itself:
//! conflict detection against other managers' `managedFields`, pruning
//! fields the applier no longer mentions, the `Compare` walk needed for
//! both) is separate, larger, not-yet-started work; this module produces
//! only the merged *value*.
//!
//! **A real, deliberate sibling of `patch::strategic_merge`, not a
//! duplicate** — the two patch kinds read genuinely different Group A
//! `FIELD_META` columns (`list_type`/`list_map_keys`/`map_type`, SSA's
//! own `x-kubernetes-*` extensions, vs. `patch_strategy`/
//! `patch_merge_key`, the older SMP pair `strategic_merge` reads), and
//! differ in two real, verified ways even though every real vendored
//! field's two annotation pairs agree in practice: a `list_type: "set"`
//! field (`ObjectMeta.finalizers`, confirmed) merges as a real
//! deduplicated union (`strategic_merge` has no equivalent concept at
//! all — SMP itself doesn't model set-typed lists, only merge-by-key
//! ones); a `map_type: "atomic"` field (`PodSpec.nodeSelector`,
//! confirmed) replaces wholesale even though it's a JSON object
//! (`strategic_merge`'s own default for *any* object-typed field is
//! always to merge recursively — it has no atomic-map concept either).
//! Multi-key associative lists (`list_map_keys` with more than one
//! entry — real upstream supports this; `strategic_merge`'s own
//! `patch_merge_key` is single-key only, a named gap that module's own
//! doc comment doesn't currently call out) are matched on *every* key
//! field here, not just the first.
//!
//! Driven by the same `codegen::field_meta_index()` table `strategic_
//! merge`/`fieldset::set_from_object` already read — no new codegen
//! needed, the SSA-specific columns were already there from Group A's
//! very first pass.

use crate::codegen;
use serde_json::{Map, Value};

/// Merges `rhs` (the incoming apply configuration) onto `lhs` (the live
/// object), both understood to be shaped like `schema` — real upstream's
/// own `TypedValue.Merge`. A type mismatch at any level (an object
/// merging against a scalar, or vice versa) falls back to `rhs` winning
/// wholesale, matching `strategic_merge::apply`'s own identical fallback
/// for the identical reason: there is no way to merge two values of
/// different shapes, so the newer one simply replaces the older one.
pub fn merge(schema: &str, lhs: &Value, rhs: &Value) -> Value {
    let (Value::Object(l), Value::Object(r)) = (lhs, rhs) else {
        return rhs.clone();
    };
    let mut result = Map::new();
    for (key, lv) in l {
        let meta = codegen::field_meta_index().get(&(schema, key.as_str())).copied();
        let merged = match r.get(key) {
            Some(rv) => merge_field(meta, lv, rv),
            None => lv.clone(),
        };
        result.insert(key.clone(), merged);
    }
    for (key, rv) in r {
        if !l.contains_key(key) {
            result.insert(key.clone(), rv.clone());
        }
    }
    Value::Object(result)
}

/// One field's own merge, given both sides are known present (a field
/// only one side has is never routed through here — [`merge`]'s own two
/// loops handle "only in `lhs`"/"only in `rhs`" by cloning that side
/// directly, matching real upstream's own leaf-merge rule: nothing to
/// combine when the other side never mentioned the field at all).
fn merge_field(meta: Option<&codegen::openapi_meta::FieldMeta>, lhs: &Value, rhs: &Value) -> Value {
    match (lhs, rhs) {
        (Value::Object(_), Value::Object(_)) if meta.and_then(|m| m.map_type) == Some("atomic") => rhs.clone(),
        (Value::Object(_), Value::Object(_)) => match meta.and_then(|m| m.ref_schema) {
            Some(next_schema) => merge(next_schema, lhs, rhs),
            None => merge_generic_map(lhs, rhs),
        },
        (Value::Array(l), Value::Array(r)) => match meta.and_then(|m| m.list_type) {
            Some("map") => Value::Array(merge_associative_list(meta.map(|m| m.list_map_keys).unwrap_or(&[]), meta.and_then(|m| m.ref_schema), l, r)),
            Some("set") => Value::Array(merge_set_list(l, r)),
            // `Some("atomic")` and everything else (unset, real
            // upstream's own default) both mean the same thing: `rhs`
            // replaces the whole list wholesale.
            _ => rhs.clone(),
        },
        // A scalar, or a type mismatch between this field's two sides —
        // `rhs` wins wholesale either way.
        _ => rhs.clone(),
    }
}

/// A generic map with no known per-key schema (`metadata.labels`, ...)
/// — merged key-by-key with no per-key metadata to recurse through,
/// real upstream's own granular-map default applying identically
/// whether the map is a real SMD "Map" type or simply untyped as far as
/// this crate's own compiled schema goes (the same fallback
/// `fieldset::set_from_object`'s own doc comment names for the
/// equivalent case).
fn merge_generic_map(lhs: &Value, rhs: &Value) -> Value {
    let (Value::Object(l), Value::Object(r)) = (lhs, rhs) else {
        return rhs.clone();
    };
    let mut result = l.clone();
    for (key, rv) in r {
        result.insert(key.clone(), rv.clone());
    }
    Value::Object(result)
}

/// `list_type: "map"` — matches `lhs`/`rhs` elements by *every* field
/// named in `list_map_keys` (real upstream supports multi-key
/// associative lists; `strategic_merge`'s own `patch_merge_key` is
/// single-key only). A matched pair merges recursively using
/// `element_schema` (falling back to a structural merge with no
/// per-field metadata when the array's own `ref_schema` is unknown,
/// same posture `strategic_merge::merge_list` already takes); an
/// unmatched `rhs` element appends. `lhs`'s own order is preserved,
/// matching elements updated in place, new elements landing at the end
/// — the identical ordering convention `strategic_merge::merge_list`
/// already established, kept consistent rather than reinvented.
fn merge_associative_list(list_map_keys: &[&str], element_schema: Option<&str>, lhs: &[Value], rhs: &[Value]) -> Vec<Value> {
    if list_map_keys.is_empty() {
        // A `list_type: map` array with no key fields at all is
        // malformed real data (real upstream always sets `list-map-keys`
        // alongside `list-type: map`) — nothing to match elements by, so
        // `rhs` replaces wholesale rather than guessing an identity.
        return rhs.to_vec();
    }
    let keys_match = |a: &Value, b: &Value| list_map_keys.iter().all(|k| a.get(*k) == b.get(*k));

    let mut result = lhs.to_vec();
    'rhs_elements: for rhs_elem in rhs {
        if !rhs_elem.is_object() {
            // Malformed real data (real upstream requires object
            // elements for an associative list) — append as-is, nothing
            // to match it by.
            result.push(rhs_elem.clone());
            continue;
        }
        for existing in result.iter_mut() {
            if existing.is_object() && keys_match(existing, rhs_elem) {
                *existing = merge(element_schema.unwrap_or(""), existing, rhs_elem);
                continue 'rhs_elements;
            }
        }
        result.push(rhs_elem.clone());
    }
    result
}

/// `list_type: "set"` — a real deduplicated union: `lhs`'s own order
/// preserved, each `rhs` element appended only if not already present
/// by value equality. Real upstream's own restriction (set-typed list
/// elements are always scalars) means no recursion is ever needed here,
/// unlike the associative-list case above.
fn merge_set_list(lhs: &[Value], rhs: &[Value]) -> Vec<Value> {
    let mut result = lhs.to_vec();
    for r in rhs {
        if !result.contains(r) {
            result.push(r.clone());
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_scalar_field_present_on_both_sides_takes_rhs() {
        let lhs = json!({"replicas": 3});
        let rhs = json!({"replicas": 5});
        assert_eq!(merge("io.k8s.api.apps.v1.DeploymentSpec", &lhs, &rhs), json!({"replicas": 5}));
    }

    #[test]
    fn a_field_only_lhs_mentions_is_kept_unchanged() {
        let lhs = json!({"replicas": 3, "minReadySeconds": 10});
        let rhs = json!({"replicas": 5});
        assert_eq!(merge("io.k8s.api.apps.v1.DeploymentSpec", &lhs, &rhs), json!({"replicas": 5, "minReadySeconds": 10}));
    }

    #[test]
    fn a_field_only_rhs_mentions_is_added() {
        let lhs = json!({"replicas": 3});
        let rhs = json!({"replicas": 3, "minReadySeconds": 10});
        assert_eq!(merge("io.k8s.api.apps.v1.DeploymentSpec", &lhs, &rhs), json!({"replicas": 3, "minReadySeconds": 10}));
    }

    #[test]
    fn a_generic_map_field_merges_key_by_key() {
        // ObjectMeta.labels: no ref_schema, plain granular map.
        let lhs = json!({"labels": {"app": "web", "tier": "frontend"}});
        let rhs = json!({"labels": {"tier": "backend", "env": "prod"}});
        let merged = merge("io.k8s.apimachinery.pkg.apis.meta.v1.ObjectMeta", &lhs, &rhs);
        assert_eq!(merged, json!({"labels": {"app": "web", "tier": "backend", "env": "prod"}}));
    }

    #[test]
    fn an_atomic_map_field_is_replaced_wholesale_not_merged() {
        // PodSpec.nodeSelector: x-kubernetes-map-type: atomic.
        let lhs = json!({"nodeSelector": {"disktype": "ssd", "region": "us-west"}});
        let rhs = json!({"nodeSelector": {"region": "us-east"}});
        let merged = merge("io.k8s.api.core.v1.PodSpec", &lhs, &rhs);
        assert_eq!(merged, json!({"nodeSelector": {"region": "us-east"}}), "an atomic map's rhs must win wholesale, not merge key-by-key");
    }

    #[test]
    fn an_associative_list_matches_by_key_and_merges_the_matched_element() {
        // PodSpec.containers: list-type: map, list-map-keys: [name].
        let lhs = json!({"containers": [{"name": "nginx", "image": "nginx:1.0"}]});
        let rhs = json!({"containers": [{"name": "nginx", "image": "nginx:2.0"}]});
        let merged = merge("io.k8s.api.core.v1.PodSpec", &lhs, &rhs);
        assert_eq!(merged, json!({"containers": [{"name": "nginx", "image": "nginx:2.0"}]}));
    }

    #[test]
    fn an_associative_list_appends_a_non_matching_element_at_the_end() {
        let lhs = json!({"containers": [{"name": "nginx", "image": "nginx:1.0"}]});
        let rhs = json!({"containers": [{"name": "sidecar", "image": "sidecar:1.0"}]});
        let merged = merge("io.k8s.api.core.v1.PodSpec", &lhs, &rhs);
        assert_eq!(merged, json!({"containers": [{"name": "nginx", "image": "nginx:1.0"}, {"name": "sidecar", "image": "sidecar:1.0"}]}));
    }

    #[test]
    fn a_set_typed_list_merges_as_a_deduplicated_union() {
        // ObjectMeta.finalizers: list-type: set.
        let lhs = json!({"finalizers": ["a.example.com/f", "b.example.com/f"]});
        let rhs = json!({"finalizers": ["b.example.com/f", "c.example.com/f"]});
        let merged = merge("io.k8s.apimachinery.pkg.apis.meta.v1.ObjectMeta", &lhs, &rhs);
        assert_eq!(merged, json!({"finalizers": ["a.example.com/f", "b.example.com/f", "c.example.com/f"]}), "b must not be duplicated");
    }

    #[test]
    fn an_atomic_list_is_replaced_wholesale() {
        // Container.command: list-type: atomic (explicit).
        let lhs = json!({"command": ["/bin/sh", "-c", "old"]});
        let rhs = json!({"command": ["/bin/sh", "-c", "new"]});
        let merged = merge("io.k8s.api.core.v1.Container", &lhs, &rhs);
        assert_eq!(merged, json!({"command": ["/bin/sh", "-c", "new"]}));
    }

    #[test]
    fn a_nested_struct_field_recurses_using_its_own_ref_schema() {
        let lhs = json!({"selector": {"matchLabels": {"app": "web", "tier": "frontend"}}});
        let rhs = json!({"selector": {"matchLabels": {"tier": "backend"}}});
        let merged = merge("io.k8s.api.apps.v1.DeploymentSpec", &lhs, &rhs);
        assert_eq!(merged, json!({"selector": {"matchLabels": {"app": "web", "tier": "backend"}}}));
    }
}
