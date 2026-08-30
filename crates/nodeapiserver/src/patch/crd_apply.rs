//! Runtime-schema Server-Side Apply for CRD-defined resources.
//!
//! Built-in resources use the compiled OpenAPI metadata consumed by
//! `fieldset`, `typed_merge`, and `updater`. A CRD's schema arrives at
//! runtime, so this module mirrors the same small set of operations over an
//! OpenAPI schema value: typed merge, field-set extraction, conflict
//! detection, pruning, and managed-field ownership transfer.

use super::fieldset::{PathElement, Set};
use super::updater::{Applied, Conflict};
use serde_json::{Map, Value};
use std::collections::{BTreeMap, BTreeSet};

pub fn apply(
    schema: &Value,
    live: &Value,
    config: &Value,
    managers: &BTreeMap<String, Set>,
    manager: &str,
    force: bool,
) -> Result<Applied, Vec<Conflict>> {
    apply_with_ignored_fields(schema, live, config, managers, manager, force, None)
}

/// [`apply`] with upstream's server-managed field exclusion applied to the
/// incoming configuration's ownership set and changed-field comparison.
pub fn apply_with_ignored_fields(
    schema: &Value,
    live: &Value,
    config: &Value,
    managers: &BTreeMap<String, Set>,
    manager: &str,
    force: bool,
    ignored_fields: Option<&Set>,
) -> Result<Applied, Vec<Conflict>> {
    let merged = merge(schema, live, config);
    let last_set = managers.get(manager).cloned();
    let new_set = filter_set(set_from_object(schema, config), ignored_fields);
    let mut all = managers.clone();
    all.insert(manager.to_string(), new_set.clone());

    let pruned = if let Some(last) = last_set.as_ref().filter(|set| !set.is_empty()) {
        let protected = all
            .iter()
            .filter(|(name, _)| name.as_str() != manager)
            .fold(Set::new(), |set, (_, other)| set.union(other));
        let to_remove = last.difference(&new_set.union(&protected));
        remove_items(schema, &merged, &to_remove)
    } else {
        merged
    };

    let comparison = filter_comparison(compare(schema, live, &pruned), ignored_fields);
    let changed = comparison.modified.union(&comparison.added);
    let mut conflicts = Vec::new();
    for (name, fields) in &all {
        if name != manager {
            let conflict = overlapping(fields, &changed);
            if !conflict.is_empty() {
                conflicts.push(Conflict {
                    manager: name.clone(),
                    fields: conflict,
                });
            }
        }
    }
    if !force && !conflicts.is_empty() {
        return Err(conflicts);
    }

    let mut result = managers.clone();
    for conflict in &conflicts {
        if let Some(fields) = result.get(&conflict.manager) {
            result.insert(conflict.manager.clone(), subtract(fields, &conflict.fields));
        }
    }
    if !comparison.removed.is_empty() {
        for fields in result.values_mut() {
            *fields = subtract(fields, &comparison.removed);
        }
    }
    result.retain(|_, fields| !fields.is_empty());
    result.insert(manager.to_string(), new_set);

    Ok(Applied {
        object: (&pruned != live).then_some(pruned),
        managers: result,
    })
}

fn filter_set(set: Set, ignored_fields: Option<&Set>) -> Set {
    match ignored_fields {
        Some(ignored) => set.recursive_difference(ignored),
        None => set,
    }
}

fn filter_comparison(comparison: Comparison, ignored_fields: Option<&Set>) -> Comparison {
    match ignored_fields {
        Some(ignored) => Comparison {
            removed: comparison.removed.recursive_difference(ignored),
            modified: comparison.modified.recursive_difference(ignored),
            added: comparison.added.recursive_difference(ignored),
        },
        None => comparison,
    }
}

fn schema_property<'a>(schema: &'a Value, name: &str) -> Option<&'a Value> {
    schema
        .get("properties")
        .and_then(Value::as_object)
        .and_then(|properties| properties.get(name))
        .or_else(|| {
            schema
                .get("additionalProperties")
                .filter(|value| value.is_object())
        })
}

fn additional_schema(schema: &Value) -> Option<&Value> {
    schema
        .get("additionalProperties")
        .filter(|value| value.is_object())
}

fn list_type(schema: &Value) -> &str {
    schema
        .get("x-kubernetes-list-type")
        .and_then(Value::as_str)
        .unwrap_or("atomic")
}

fn list_keys(schema: &Value) -> Vec<&str> {
    schema
        .get("x-kubernetes-list-map-keys")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect()
}

fn is_atomic_map(schema: &Value) -> bool {
    schema.get("x-kubernetes-map-type").and_then(Value::as_str) == Some("atomic")
}

pub fn merge(schema: &Value, live: &Value, config: &Value) -> Value {
    let (Value::Object(live), Value::Object(config)) = (live, config) else {
        return config.clone();
    };
    let mut result = live.clone();
    for (name, incoming) in config {
        let merged = match (result.get(name), schema_property(schema, name)) {
            (Some(existing), Some(field_schema)) => merge_field(field_schema, existing, incoming),
            // A CRD's structural schema normally describes `spec` and
            // `status` only. The effective Kubernetes schema still treats
            // top-level `metadata` as a granular object, so applying a
            // configuration containing only `metadata.name` must not erase
            // the live UID, timestamps, labels, or managedFields.
            (Some(existing), None) if name == "metadata" => merge_untyped(existing, incoming),
            _ => incoming.clone(),
        };
        result.insert(name.clone(), merged);
    }
    Value::Object(result)
}

fn merge_untyped(live: &Value, config: &Value) -> Value {
    let (Value::Object(live), Value::Object(config)) = (live, config) else {
        return config.clone();
    };
    let mut result = live.clone();
    for (name, incoming) in config {
        let merged = match result.get(name) {
            Some(existing) if existing.is_object() && incoming.is_object() => {
                merge_untyped(existing, incoming)
            }
            _ => incoming.clone(),
        };
        result.insert(name.clone(), merged);
    }
    Value::Object(result)
}

fn merge_field(schema: &Value, live: &Value, config: &Value) -> Value {
    match (live, config) {
        (Value::Object(_), Value::Object(_)) if is_atomic_map(schema) => config.clone(),
        (Value::Object(_), Value::Object(_)) => merge(schema, live, config),
        (Value::Array(live), Value::Array(config)) => match list_type(schema) {
            "map" => merge_map_list(schema, live, config),
            "set" => merge_set_list(live, config),
            _ => Value::Array(config.clone()),
        },
        _ => config.clone(),
    }
}

fn merge_map_list(schema: &Value, live: &[Value], config: &[Value]) -> Value {
    let keys = list_keys(schema);
    if keys.is_empty() {
        return Value::Array(config.to_vec());
    }
    let item_schema = schema.get("items").unwrap_or(&Value::Null);
    let mut result = live.to_vec();
    'incoming: for item in config {
        for existing in &mut result {
            if same_keys(existing, item, &keys) {
                *existing = merge(item_schema, existing, item);
                continue 'incoming;
            }
        }
        result.push(item.clone());
    }
    Value::Array(result)
}

fn merge_set_list(live: &[Value], config: &[Value]) -> Value {
    let mut result = live.to_vec();
    for item in config {
        if !result.contains(item) {
            result.push(item.clone());
        }
    }
    Value::Array(result)
}

fn same_keys(left: &Value, right: &Value, keys: &[&str]) -> bool {
    let (Some(left), Some(right)) = (left.as_object(), right.as_object()) else {
        return false;
    };
    keys.iter()
        .all(|key| left.get(*key).is_some() && left.get(*key) == right.get(*key))
}

pub fn set_from_object(schema: &Value, value: &Value) -> Set {
    let mut set = Set::new();
    let mut path = Vec::new();
    collect_object(schema, value, &mut path, &mut set);
    set
}

fn collect_object(schema: &Value, value: &Value, path: &mut Vec<PathElement>, set: &mut Set) {
    let Some(object) = value.as_object() else {
        return;
    };
    for (name, value) in object {
        path.push(PathElement::Field(name.clone()));
        if name == "metadata" && value.is_object() && schema_property(schema, name).is_none() {
            collect_untyped(value, path, set);
        } else if let Some(field_schema) = schema_property(schema, name) {
            collect_field(field_schema, value, path, set);
        } else {
            set.insert(path);
        }
        path.pop();
    }
}

fn collect_untyped(value: &Value, path: &mut Vec<PathElement>, set: &mut Set) {
    match value {
        Value::Object(object) if object.is_empty() => set.insert(path),
        Value::Object(object) => {
            for (name, value) in object {
                path.push(PathElement::Field(name.clone()));
                collect_untyped(value, path, set);
                path.pop();
            }
        }
        Value::Array(_) | Value::String(_) | Value::Number(_) | Value::Bool(_) | Value::Null => {
            set.insert(path)
        }
    }
}

fn collect_field(schema: &Value, value: &Value, path: &mut Vec<PathElement>, set: &mut Set) {
    match value {
        Value::Object(_) if is_atomic_map(schema) => set.insert(path),
        Value::Object(object) => {
            if schema.get("properties").is_some() {
                collect_object(schema, value, path, set);
            } else if let Some(value_schema) = additional_schema(schema) {
                for (name, value) in object {
                    path.push(PathElement::Field(name.clone()));
                    collect_field(value_schema, value, path, set);
                    path.pop();
                }
            } else if object.is_empty() {
                set.insert(path);
            } else {
                for name in object.keys() {
                    path.push(PathElement::Field(name.clone()));
                    set.insert(path);
                    path.pop();
                }
            }
        }
        Value::Array(items) => match list_type(schema) {
            "map" => {
                let keys = list_keys(schema);
                let item_schema = schema.get("items").unwrap_or(&Value::Null);
                for item in items {
                    let Some(object) = item.as_object() else {
                        continue;
                    };
                    let mut key_fields = keys
                        .iter()
                        .filter_map(|key| {
                            object
                                .get(*key)
                                .map(|value| ((*key).to_string(), value.clone()))
                        })
                        .collect::<Vec<_>>();
                    key_fields.sort_by(|left, right| left.0.cmp(&right.0));
                    path.push(PathElement::Key(key_fields));
                    if item_schema.is_null() {
                        set.insert(path);
                    } else {
                        collect_object(item_schema, item, path, set);
                    }
                    path.pop();
                }
            }
            "set" => {
                for item in items {
                    path.push(PathElement::Value(item.clone()));
                    set.insert(path);
                    path.pop();
                }
            }
            _ => set.insert(path),
        },
        _ => set.insert(path),
    }
}

#[derive(Default)]
struct Comparison {
    removed: Set,
    modified: Set,
    added: Set,
}

fn compare(schema: &Value, live: &Value, candidate: &Value) -> Comparison {
    let mut result = Comparison::default();
    let mut path = Vec::new();
    compare_object(schema, live, candidate, &mut path, &mut result);
    result
}

/// Exposes the CRD schema comparison in the same public shape as the
/// compiled-schema comparator. The version-aware updater uses this after it
/// has converted a live/candidate pair into a manager's recorded version.
pub fn compare_for_managed_fields(schema: &Value, live: &Value, candidate: &Value) -> crate::patch::typed_compare::Comparison {
    let comparison = compare(schema, live, candidate);
    crate::patch::typed_compare::Comparison {
        removed: comparison.removed,
        modified: comparison.modified,
        added: comparison.added,
    }
}

fn compare_object(
    schema: &Value,
    live: &Value,
    candidate: &Value,
    path: &mut Vec<PathElement>,
    result: &mut Comparison,
) {
    let (Some(live), Some(candidate)) = (live.as_object(), candidate.as_object()) else {
        if live != candidate {
            result.modified.insert(path);
        }
        return;
    };
    let mut names = BTreeSet::new();
    names.extend(live.keys());
    names.extend(candidate.keys());
    for name in names {
        path.push(PathElement::Field(name.clone()));
        let field_schema = schema_property(schema, name).unwrap_or(&Value::Null);
        match (live.get(name), candidate.get(name)) {
            (Some(old), Some(new)) => compare_field(field_schema, old, new, path, result),
            (Some(old), None) => collect_changed(field_schema, old, path, &mut result.removed),
            (None, Some(new)) => collect_changed(field_schema, new, path, &mut result.added),
            (None, None) => {}
        }
        path.pop();
    }
}

fn compare_field(
    schema: &Value,
    live: &Value,
    candidate: &Value,
    path: &mut Vec<PathElement>,
    result: &mut Comparison,
) {
    if live == candidate {
        return;
    }
    match (live, candidate) {
        (Value::Object(_), Value::Object(_)) if !is_atomic_map(schema) => {
            compare_object(schema, live, candidate, path, result)
        }
        (Value::Array(live), Value::Array(candidate)) if list_type(schema) == "map" => {
            let keys = list_keys(schema);
            if keys.is_empty() {
                result.modified.insert(path);
                return;
            }
            let item_schema = schema.get("items").unwrap_or(&Value::Null);
            for item in live {
                if !candidate.iter().any(|other| same_keys(item, other, &keys)) {
                    let mut item_path = path.clone();
                    item_path.push(PathElement::Key(key_values(item, &keys)));
                    collect_changed(item_schema, item, &item_path, &mut result.removed);
                }
            }
            for item in candidate {
                let Some(old) = live.iter().find(|other| same_keys(item, other, &keys)) else {
                    let mut item_path = path.clone();
                    item_path.push(PathElement::Key(key_values(item, &keys)));
                    collect_changed(item_schema, item, &item_path, &mut result.added);
                    continue;
                };
                let mut item_path = path.clone();
                item_path.push(PathElement::Key(key_values(item, &keys)));
                compare_object(item_schema, old, item, &mut item_path, result);
            }
        }
        (Value::Array(live), Value::Array(candidate)) if list_type(schema) == "set" => {
            for item in live {
                if !candidate.contains(item) {
                    path.push(PathElement::Value(item.clone()));
                    result.removed.insert(path);
                    path.pop();
                }
            }
            for item in candidate {
                if !live.contains(item) {
                    path.push(PathElement::Value(item.clone()));
                    result.added.insert(path);
                    path.pop();
                }
            }
        }
        _ => {
            result.modified.insert(path);
        }
    }
}

fn key_values(item: &Value, keys: &[&str]) -> Vec<(String, Value)> {
    let Some(object) = item.as_object() else {
        return Vec::new();
    };
    let mut values = keys
        .iter()
        .filter_map(|key| {
            object
                .get(*key)
                .map(|value| ((*key).to_string(), value.clone()))
        })
        .collect::<Vec<_>>();
    values.sort_by(|left, right| left.0.cmp(&right.0));
    values
}

fn collect_changed(schema: &Value, value: &Value, path: &[PathElement], set: &mut Set) {
    set.insert(path);
    match value {
        Value::Object(object) if !is_atomic_map(schema) => {
            for (name, value) in object {
                let mut child = path.to_vec();
                child.push(PathElement::Field(name.clone()));
                collect_changed(
                    schema_property(schema, name).unwrap_or(&Value::Null),
                    value,
                    &child,
                    set,
                );
            }
        }
        Value::Array(items) if list_type(schema) == "map" => {
            let keys = list_keys(schema);
            let item_schema = schema.get("items").unwrap_or(&Value::Null);
            for item in items {
                let mut child = path.to_vec();
                child.push(PathElement::Key(key_values(item, &keys)));
                collect_changed(item_schema, item, &child, set);
            }
        }
        Value::Array(items) if list_type(schema) == "set" => {
            for item in items {
                let mut child = path.to_vec();
                child.push(PathElement::Value(item.clone()));
                set.insert(&child);
            }
        }
        _ => {}
    }
}

pub fn remove_items(schema: &Value, value: &Value, to_remove: &Set) -> Value {
    let Some(object) = value.as_object() else {
        return value.clone();
    };
    let mut result = Map::new();
    for (name, value) in object {
        let path = PathElement::Field(name.clone());
        if to_remove.members.contains(&path) {
            continue;
        }
        if let Some(children) = to_remove.children.get(&path).filter(|set| !set.is_empty()) {
            let field_schema = schema_property(schema, name).unwrap_or(&Value::Null);
            let removed = remove_field(field_schema, value, children);
            result.insert(name.clone(), removed);
        } else {
            result.insert(name.clone(), value.clone());
        }
    }
    Value::Object(result)
}

fn remove_field(schema: &Value, value: &Value, to_remove: &Set) -> Value {
    match value {
        Value::Object(_) if is_atomic_map(schema) => Value::Null,
        Value::Object(_) => {
            let removed = remove_items(schema, value, to_remove);
            if removed.as_object().is_some_and(Map::is_empty) {
                Value::Object(Map::new())
            } else {
                removed
            }
        }
        Value::Array(items) if list_type(schema) == "map" => {
            let keys = list_keys(schema);
            let item_schema = schema.get("items").unwrap_or(&Value::Null);
            let mut result = Vec::new();
            for item in items {
                let path = PathElement::Key(key_values(item, &keys));
                if to_remove.members.contains(&path) {
                    continue;
                }
                if let Some(children) = to_remove.children.get(&path).filter(|set| !set.is_empty())
                {
                    result.push(remove_field(item_schema, item, children));
                } else {
                    result.push(item.clone());
                }
            }
            Value::Array(result)
        }
        Value::Array(items) if list_type(schema) == "set" => Value::Array(
            items
                .iter()
                .filter(|item| {
                    !to_remove
                        .members
                        .contains(&PathElement::Value((*item).clone()))
                })
                .cloned()
                .collect(),
        ),
        Value::Array(_) => Value::Null,
        _ => value.clone(),
    }
}

fn overlapping(left: &Set, right: &Set) -> Set {
    let mut result = Set::new();
    for member in &left.members {
        if right.members.contains(member) || right.children.contains_key(member) {
            result.members.push(member.clone());
        }
    }
    for (path, children) in &left.children {
        if right.members.contains(path) {
            result.children.insert(path.clone(), children.clone());
        } else if let Some(other) = right.children.get(path) {
            let overlap = overlapping(children, other);
            if !overlap.is_empty() {
                result.children.insert(path.clone(), overlap);
            }
        }
    }
    result.members.sort();
    result
}

fn subtract(left: &Set, taken: &Set) -> Set {
    let mut result = Set::new();
    result.members = left
        .members
        .iter()
        .filter(|member| !taken.members.contains(member) && !taken.children.contains_key(member))
        .cloned()
        .collect();
    for (path, children) in &left.children {
        if taken.members.contains(path) {
            continue;
        }
        let remaining = match taken.children.get(path) {
            Some(taken_children) => subtract(children, taken_children),
            None => children.clone(),
        };
        if !remaining.is_empty() {
            result.children.insert(path.clone(), remaining);
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn schema() -> Value {
        json!({
            "type": "object",
            "properties": {
                "spec": {"type": "object", "properties": {
                    "color": {"type": "string"},
                    "ports": {"type": "array", "x-kubernetes-list-type": "map", "x-kubernetes-list-map-keys": ["name"], "items": {"type": "object", "properties": {"name": {"type": "string"}, "port": {"type": "integer"}}}}
                }}
            }
        })
    }

    fn path(fields: &[&str]) -> Vec<PathElement> {
        fields
            .iter()
            .map(|field| PathElement::Field((*field).to_string()))
            .collect()
    }

    #[test]
    fn merges_a_crd_object_and_tracks_runtime_fields() {
        let result = apply(
            &schema(),
            &json!({"spec": {"color": "red", "ports": [{"name": "http", "port": 80}]}}),
            &json!({"spec": {"color": "blue", "ports": [{"name": "http", "port": 8080}]}}),
            &BTreeMap::new(),
            "kubectl-client-side-apply",
            false,
        )
        .unwrap();
        let object = result.object.unwrap();
        assert_eq!(object["spec"]["color"], "blue");
        assert_eq!(object["spec"]["ports"][0]["port"], 8080);
        assert!(result.managers["kubectl-client-side-apply"].has(&[
            PathElement::Field("spec".to_string()),
            PathElement::Field("color".to_string()),
        ]));
    }

    #[test]
    fn rejects_a_runtime_schema_conflict_without_force() {
        let prior = set_from_object(&schema(), &json!({"spec": {"color": "red"}}));
        let managers = BTreeMap::from([("first".to_string(), prior)]);
        let result = apply(
            &schema(),
            &json!({"spec": {"color": "red"}}),
            &json!({"spec": {"color": "blue"}}),
            &managers,
            "second",
            false,
        );
        assert_eq!(result.unwrap_err().len(), 1);
    }

    #[test]
    fn apply_ignores_server_managed_fields() {
        let mut schema = schema();
        schema["properties"]["status"] = json!({
            "type": "object",
            "properties": {"ready": {"type": "boolean"}}
        });
        let mut ignored = Set::new();
        ignored.insert(&path(&["status"]));
        let mut status_manager = Set::new();
        status_manager.insert(&path(&["status", "ready"]));
        let managers = BTreeMap::from([("status-controller".to_string(), status_manager)]);

        let result = apply_with_ignored_fields(
            &schema,
            &json!({"spec": {"color": "red"}, "status": {"ready": false}}),
            &json!({"spec": {"color": "blue"}, "status": {"ready": true}}),
            &managers,
            "kubectl-apply",
            false,
            Some(&ignored),
        )
        .expect("status is ignored, so it must not conflict");

        assert!(result.managers["status-controller"].has(&path(&["status", "ready"])));
        assert!(result.managers["kubectl-apply"].has(&path(&["spec", "color"])));
        assert!(!result.managers["kubectl-apply"].has(&path(&["status", "ready"])));
    }

    #[test]
    fn preserves_server_metadata_when_apply_config_only_names_an_object() {
        let result = apply(
            &schema(),
            &json!({
                "apiVersion": "example.test/v1",
                "kind": "Widget",
                "metadata": {
                    "name": "one",
                    "uid": "uid-one",
                    "creationTimestamp": "2026-08-28T00:00:00Z",
                    "managedFields": []
                },
                "spec": {"color": "red"}
            }),
            &json!({
                "apiVersion": "example.test/v1",
                "kind": "Widget",
                "metadata": {"name": "one"},
                "spec": {"color": "blue"}
            }),
            &BTreeMap::new(),
            "kubectl-client-side-apply",
            false,
        )
        .unwrap();
        let object = result.object.unwrap();
        assert_eq!(object["metadata"]["uid"], "uid-one");
        assert_eq!(
            object["metadata"]["creationTimestamp"],
            "2026-08-28T00:00:00Z"
        );
        assert!(result.managers["kubectl-client-side-apply"].has(&[
            PathElement::Field("metadata".to_string()),
            PathElement::Field("name".to_string()),
        ]));
    }
}
