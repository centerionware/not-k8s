//! Server-Side Apply's own real *diff* — `typed.TypedValue.Compare`,
//! ported (`sigs.k8s.io/structured-merge-diff/v6/typed/compare.go`,
//! fetched and read directly). `Updater.Apply`'s own conflict detection
//! needs to know exactly which fields an apply actually changed (not
//! just which fields the incoming config mentions — `typed_merge::merge`
//! already answers that structurally, but not which of those already
//! matched the live object's own existing value) before it can ask "is
//! any of that intersecting a field another manager owns" — this module
//! is that comparison, the third of `merge.Updater`'s own real
//! prerequisites (`fieldset::set_from_object`, `typed_merge::merge`,
//! this) to land, `Updater.Apply` itself still not started.
//!
//! Reads the exact same SSA-specific `FIELD_META` columns
//! (`list_type`/`list_map_keys`/`map_type`/`ref_schema`) every other
//! module in this arc already does — no new codegen, the fourth
//! primitive in a row to reuse Group A's table for free.
//!
//! # Real semantics, confirmed against upstream's own source
//!
//! A field present on only one side is **both** inserted at its own
//! path *and* recursed into (every leaf beneath it inserted too, at
//! full depth) — confirmed directly against `compareWalker.compare`'s
//! own `!w.inLeaf` insert *after* `handleAtom` has already recursed
//! through the one-sided subtree. This isn't redundant: `fieldpath::Set`
//! genuinely models "this exact path is itself a member, *and* it has
//! tracked children" as one real, valid, distinct state (the `"."`
//! marker `fieldset`'s own doc comment already covers) — a wholly new
//! `containers: [...]` field being added needs both "containers itself
//! was added" *and* "containers[0].image was added" tracked, and this
//! module's own `Set::insert` already handles a path being inserted at
//! multiple depths correctly.
//!
//! A field present on both sides recurses using the identical
//! atomic/associative-map/set-list decisions `fieldset::set_from_object`/
//! `typed_merge::merge` already encode; a genuine leaf (scalar, atomic
//! list, atomic map, a matched associative-list element with no further
//! difference) inserts into `modified` only when the two values actually
//! differ (`serde_json::Value`'s own `PartialEq`, not upstream's own
//! `value.EqualsUsing` — an internal detail of *which* deep-equality
//! check runs, not a semantic difference for this crate's own plain-JSON
//! representation).
//!
//! # Named, deliberate simplification
//!
//! Real upstream's own `visitListItems` has a four-way branch for
//! **duplicate elements sharing the same associative-list identity**
//! (comparing the duplicate runs as if they were atomic) — genuinely
//! invalid data for a real associative list (upstream's own validation
//! would reject it elsewhere), not a case any real vendored object ever
//! produces. This module takes the first occurrence of a repeated
//! identity on either side and silently ignores the rest, rather than
//! porting that four-way duplicate-handling switch — named honestly,
//! not silently assumed away.

use crate::codegen;
use crate::patch::fieldset::{PathElement, Set};
use serde_json::Value;
use std::collections::BTreeSet;

/// Real upstream's own `Comparison` — three disjoint `Set`s (a field
/// never appears in more than one; if all three are empty, the two
/// objects compared equal).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Comparison {
    pub removed: Set,
    pub modified: Set,
    pub added: Set,
}

impl Comparison {
    /// Real upstream's own `Comparison.IsSame`.
    pub fn is_same(&self) -> bool {
        self.removed.is_empty() && self.modified.is_empty() && self.added.is_empty()
    }
}

/// Compares `lhs` (the "before") against `rhs` (the "after"), both
/// understood to be shaped like `schema` — real upstream's own
/// `TypedValue.Compare`.
pub fn compare(schema: &str, lhs: &Value, rhs: &Value) -> Comparison {
    let mut c = Comparison::default();
    let mut path = Vec::new();
    compare_objects(schema, lhs, rhs, &mut path, &mut c);
    c
}

fn compare_objects(schema: &str, lhs: &Value, rhs: &Value, path: &mut Vec<PathElement>, c: &mut Comparison) {
    let (Value::Object(l), Value::Object(r)) = (lhs, rhs) else {
        // Shouldn't happen at the real root (a k8s object is always a
        // JSON object) -- handled gracefully as a single leaf compare
        // rather than assumed unreachable.
        compare_leaf(lhs, rhs, path, c);
        return;
    };
    let mut keys: BTreeSet<&String> = l.keys().collect();
    keys.extend(r.keys());
    for key in keys {
        path.push(PathElement::Field(key.clone()));
        let meta = codegen::field_meta_index().get(&(schema, key.as_str())).copied();
        match (l.get(key), r.get(key)) {
            (Some(lv), Some(rv)) => compare_present(meta, lv, rv, path, c),
            (Some(lv), None) => mark_one_sided(meta, lv, &mut c.removed, path),
            (None, Some(rv)) => mark_one_sided(meta, rv, &mut c.added, path),
            (None, None) => unreachable!("key came from the union of both maps' own keys"),
        }
        path.pop();
    }
}

/// A field present on only one side — inserts `path` itself into
/// `target` (`removed`/`added`), then recurses through the value's own
/// structure (using the identical atomic/associative/set rules
/// `collect_field_value` in `fieldset::set_from_object` already
/// establishes) inserting every leaf beneath it too. See this module's
/// own doc comment for why both insertions are real, not redundant.
fn mark_one_sided(meta: Option<&codegen::openapi_meta::FieldMeta>, value: &Value, target: &mut Set, path: &mut Vec<PathElement>) {
    target.insert(path);
    match value {
        Value::Object(map) if meta.and_then(|m| m.map_type) != Some("atomic") => match meta.and_then(|m| m.ref_schema) {
            Some(next_schema) => {
                for (key, v) in map {
                    path.push(PathElement::Field(key.clone()));
                    let next_meta = codegen::field_meta_index().get(&(next_schema, key.as_str())).copied();
                    mark_one_sided(next_meta, v, target, path);
                    path.pop();
                }
            }
            None => {
                for key in map.keys() {
                    path.push(PathElement::Field(key.clone()));
                    target.insert(path);
                    path.pop();
                }
            }
        },
        Value::Array(elements) => match meta.and_then(|m| m.list_type) {
            Some("map") => {
                let list_map_keys = meta.map(|m| m.list_map_keys).unwrap_or(&[]);
                let element_schema = meta.and_then(|m| m.ref_schema);
                for element in elements {
                    let Value::Object(obj) = element else { continue };
                    let mut key_fields: Vec<(String, Value)> = list_map_keys.iter().filter_map(|k| obj.get(*k).map(|v| (k.to_string(), v.clone()))).collect();
                    key_fields.sort_by(|a, b| a.0.cmp(&b.0));
                    path.push(PathElement::Key(key_fields));
                    mark_one_sided_element(element_schema, element, target, path);
                    path.pop();
                }
            }
            Some("set") => {
                for element in elements {
                    path.push(PathElement::Value(element.clone()));
                    target.insert(path);
                    path.pop();
                }
            }
            // Atomic/unset list, or a non-object/non-array value at this
            // point (a plain scalar) -- `target.insert(path)` at the top
            // of this function already covers it; nothing more to do.
            _ => {}
        },
        _ => {}
    }
}

/// One whole associative-list element, present on only one side — marks
/// its own `k:{...}` path as owned, then (when the element schema is
/// known) recurses through its own fields the same way [`mark_one_sided`]
/// does for a plain object field. Shared by [`mark_one_sided`]'s own
/// `list_type: "map"` case (the whole array is one-sided) and
/// [`compare_associative_list`]'s one-sided-*element* case (the array
/// itself is present on both sides, but this one keyed element only
/// exists on one of them) — the same real recursion either way, so it's
/// factored out rather than duplicated.
fn mark_one_sided_element(element_schema: Option<&str>, element: &Value, target: &mut Set, path: &mut Vec<PathElement>) {
    target.insert(path);
    if let (Some(s), Value::Object(obj)) = (element_schema, element) {
        for (key, v) in obj {
            path.push(PathElement::Field(key.clone()));
            let next_meta = codegen::field_meta_index().get(&(s, key.as_str())).copied();
            mark_one_sided(next_meta, v, target, path);
            path.pop();
        }
    }
}

/// A field present on both sides — the identical atomic/associative/set
/// decisions [`mark_one_sided`] and `typed_merge::merge_field` both make,
/// applied here to produce `modified`/`added`/`removed` instead of a
/// merged value or an ownership set.
fn compare_present(meta: Option<&codegen::openapi_meta::FieldMeta>, lhs: &Value, rhs: &Value, path: &mut Vec<PathElement>, c: &mut Comparison) {
    match (lhs, rhs) {
        (Value::Object(_), Value::Object(_)) if meta.and_then(|m| m.map_type) == Some("atomic") => compare_leaf(lhs, rhs, path, c),
        (Value::Object(l), Value::Object(r)) => match meta.and_then(|m| m.ref_schema) {
            Some(next_schema) => compare_objects(next_schema, lhs, rhs, path, c),
            None => {
                let mut keys: BTreeSet<&String> = l.keys().collect();
                keys.extend(r.keys());
                for key in keys {
                    path.push(PathElement::Field(key.clone()));
                    match (l.get(key), r.get(key)) {
                        (Some(lv), Some(rv)) => compare_leaf(lv, rv, path, c),
                        (Some(lv), None) => mark_one_sided(None, lv, &mut c.removed, path),
                        (None, Some(rv)) => mark_one_sided(None, rv, &mut c.added, path),
                        (None, None) => unreachable!(),
                    }
                    path.pop();
                }
            }
        },
        (Value::Array(l), Value::Array(r)) => match meta.and_then(|m| m.list_type) {
            Some("map") => compare_associative_list(meta.map(|m| m.list_map_keys).unwrap_or(&[]), meta.and_then(|m| m.ref_schema), l, r, path, c),
            Some("set") => compare_set_list(l, r, path, c),
            _ => compare_leaf(lhs, rhs, path, c),
        },
        _ => compare_leaf(lhs, rhs, path, c),
    }
}

/// A genuine leaf comparison — real upstream's own `doLeaf`: `modified`
/// only when the two values actually differ.
fn compare_leaf(lhs: &Value, rhs: &Value, path: &mut Vec<PathElement>, c: &mut Comparison) {
    if lhs != rhs {
        c.modified.insert(path);
    }
}

fn compare_associative_list(list_map_keys: &[&str], element_schema: Option<&str>, lhs: &[Value], rhs: &[Value], path: &mut Vec<PathElement>, c: &mut Comparison) {
    if list_map_keys.is_empty() {
        compare_leaf(&Value::Array(lhs.to_vec()), &Value::Array(rhs.to_vec()), path, c);
        return;
    }
    let key_of = |v: &Value| -> Option<Vec<(String, Value)>> {
        let obj = v.as_object()?;
        let mut fields: Vec<(String, Value)> = list_map_keys.iter().filter_map(|k| obj.get(*k).map(|v| (k.to_string(), v.clone()))).collect();
        fields.sort_by(|a, b| a.0.cmp(&b.0));
        Some(fields)
    };
    // First occurrence per identity on each side wins -- see this
    // module's own doc comment on the real, named duplicate-handling
    // simplification.
    let mut l_by_key: Vec<(Vec<(String, Value)>, &Value)> = Vec::new();
    for elem in lhs {
        if let Some(k) = key_of(elem) {
            if !l_by_key.iter().any(|(ek, _)| ek == &k) {
                l_by_key.push((k, elem));
            }
        }
    }
    let mut r_by_key: Vec<(Vec<(String, Value)>, &Value)> = Vec::new();
    for elem in rhs {
        if let Some(k) = key_of(elem) {
            if !r_by_key.iter().any(|(ek, _)| ek == &k) {
                r_by_key.push((k, elem));
            }
        }
    }
    let mut all_keys: Vec<Vec<(String, Value)>> = l_by_key.iter().map(|(k, _)| k.clone()).collect();
    for (k, _) in &r_by_key {
        if !all_keys.contains(k) {
            all_keys.push(k.clone());
        }
    }
    for key in all_keys {
        let lv = l_by_key.iter().find(|(k, _)| k == &key).map(|(_, v)| *v);
        let rv = r_by_key.iter().find(|(k, _)| k == &key).map(|(_, v)| *v);
        path.push(PathElement::Key(key));
        match (lv, rv) {
            (Some(l), Some(r)) => match element_schema {
                Some(s) => compare_objects(s, l, r, path, c),
                None => compare_leaf(l, r, path, c),
            },
            (Some(l), None) => mark_one_sided_element(element_schema, l, &mut c.removed, path),
            (None, Some(r)) => mark_one_sided_element(element_schema, r, &mut c.added, path),
            (None, None) => unreachable!(),
        }
        path.pop();
    }
}

fn compare_set_list(lhs: &[Value], rhs: &[Value], path: &mut Vec<PathElement>, c: &mut Comparison) {
    for l in lhs {
        if !rhs.contains(l) {
            path.push(PathElement::Value(l.clone()));
            c.removed.insert(path);
            path.pop();
        }
    }
    for r in rhs {
        if !lhs.contains(r) {
            path.push(PathElement::Value(r.clone()));
            c.added.insert(path);
            path.pop();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn identical_objects_compare_as_same() {
        let v = json!({"replicas": 3});
        let c = compare("io.k8s.api.apps.v1.DeploymentSpec", &v, &v);
        assert!(c.is_same());
    }

    #[test]
    fn a_changed_scalar_is_modified() {
        let lhs = json!({"replicas": 3});
        let rhs = json!({"replicas": 5});
        let c = compare("io.k8s.api.apps.v1.DeploymentSpec", &lhs, &rhs);
        assert!(c.modified.has(&[PathElement::Field("replicas".to_string())]));
        assert!(c.added.is_empty());
        assert!(c.removed.is_empty());
    }

    #[test]
    fn a_new_scalar_field_is_added() {
        let lhs = json!({"replicas": 3});
        let rhs = json!({"replicas": 3, "minReadySeconds": 10});
        let c = compare("io.k8s.api.apps.v1.DeploymentSpec", &lhs, &rhs);
        assert!(c.added.has(&[PathElement::Field("minReadySeconds".to_string())]));
        assert!(c.modified.is_empty());
    }

    #[test]
    fn a_removed_scalar_field_is_removed() {
        let lhs = json!({"replicas": 3, "minReadySeconds": 10});
        let rhs = json!({"replicas": 3});
        let c = compare("io.k8s.api.apps.v1.DeploymentSpec", &lhs, &rhs);
        assert!(c.removed.has(&[PathElement::Field("minReadySeconds".to_string())]));
    }

    #[test]
    fn a_wholly_new_nested_subtree_is_added_at_both_its_own_root_and_every_leaf() {
        // PodSpec.containers is new entirely -- real upstream's own
        // "both the parent path and every leaf beneath it" rule.
        let lhs = json!({});
        let rhs = json!({"containers": [{"name": "nginx", "image": "nginx:latest"}]});
        let c = compare("io.k8s.api.core.v1.PodSpec", &lhs, &rhs);
        assert!(c.added.has(&[PathElement::Field("containers".to_string())]), "the parent path itself must be added");
        let key = PathElement::Key(vec![("name".to_string(), json!("nginx"))]);
        assert!(c.added.has(&[PathElement::Field("containers".to_string()), key.clone(), PathElement::Field("image".to_string())]));
        assert!(c.added.has(&[PathElement::Field("containers".to_string()), key, PathElement::Field("name".to_string())]));
    }

    #[test]
    fn an_atomic_map_field_that_changed_at_all_is_one_modified_leaf() {
        let lhs = json!({"nodeSelector": {"disktype": "ssd"}});
        let rhs = json!({"nodeSelector": {"disktype": "hdd"}});
        let c = compare("io.k8s.api.core.v1.PodSpec", &lhs, &rhs);
        assert!(c.modified.has(&[PathElement::Field("nodeSelector".to_string())]), "an atomic map is one leaf, not per-key diffs");
        assert!(!c.modified.has(&[PathElement::Field("nodeSelector".to_string()), PathElement::Field("disktype".to_string())]));
    }

    #[test]
    fn an_associative_list_element_matched_by_key_compares_its_own_fields() {
        let lhs = json!({"containers": [{"name": "nginx", "image": "nginx:1.0"}]});
        let rhs = json!({"containers": [{"name": "nginx", "image": "nginx:2.0"}]});
        let c = compare("io.k8s.api.core.v1.PodSpec", &lhs, &rhs);
        let key = PathElement::Key(vec![("name".to_string(), json!("nginx"))]);
        assert!(c.modified.has(&[PathElement::Field("containers".to_string()), key, PathElement::Field("image".to_string())]));
    }

    #[test]
    fn a_set_typed_list_reports_only_the_real_delta_elements() {
        let lhs = json!({"finalizers": ["a.example.com/f", "b.example.com/f"]});
        let rhs = json!({"finalizers": ["b.example.com/f", "c.example.com/f"]});
        let c = compare("io.k8s.apimachinery.pkg.apis.meta.v1.ObjectMeta", &lhs, &rhs);
        assert!(c.removed.has(&[PathElement::Field("finalizers".to_string()), PathElement::Value(json!("a.example.com/f"))]));
        assert!(c.added.has(&[PathElement::Field("finalizers".to_string()), PathElement::Value(json!("c.example.com/f"))]));
        assert!(!c.modified.has(&[PathElement::Field("finalizers".to_string()), PathElement::Value(json!("b.example.com/f"))]), "an element present on both sides is not itself a diff");
    }

    #[test]
    fn an_atomic_list_that_changed_at_all_is_one_modified_leaf() {
        let lhs = json!({"command": ["/bin/sh", "-c", "old"]});
        let rhs = json!({"command": ["/bin/sh", "-c", "new"]});
        let c = compare("io.k8s.api.core.v1.Container", &lhs, &rhs);
        assert!(c.modified.has(&[PathElement::Field("command".to_string())]));
    }
}
