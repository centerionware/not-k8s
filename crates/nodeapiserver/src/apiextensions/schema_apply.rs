//! Server-Side Apply for CRD-defined resources.
//!
//! Built-in objects use the compiled `FIELD_META` table and the generic
//! `patch::updater` implementation. A CRD's schema is supplied at runtime,
//! so this module mirrors the same small set of operations over a JSON
//! schema: associative lists use their declared map keys, set lists use
//! value ownership, and unannotated/atomic lists are owned as one field.
//! The returned field sets are the same `fieldsV1` representation used by
//! built-in SSA, which keeps managed-fields behavior consistent across both
//! resource classes.

use crate::patch::fieldset::{PathElement, Set};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, PartialEq)]
pub struct Applied {
    pub object: Option<Value>,
    pub managers: BTreeMap<String, Set>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Conflict {
    pub manager: String,
    pub fields: Set,
}

/// Applies a CRD schema's field ownership and merge rules to one object.
/// `managers` contains the existing `managedFields` entries, including the
/// applying manager when it has applied before.
pub fn apply(
    schema: &Value,
    live: &Value,
    config: &Value,
    managers: &BTreeMap<String, Set>,
    manager: &str,
    force: bool,
) -> Result<Applied, Vec<Conflict>> {
    let merged = merge(schema, live, config);
    let new_set = set_from_object(schema, config);
    let last_set = managers.get(manager).cloned().unwrap_or_default();

    let mut changed = Set::new();
    let mut removed = Set::new();
    diff(
        schema,
        live,
        &merged,
        &mut Vec::new(),
        &mut changed,
        &mut removed,
    );

    let mut conflicts = Vec::new();
    for (other_manager, other_set) in managers {
        if other_manager == manager {
            continue;
        }
        let fields = other_set.intersection(&changed);
        if !fields.is_empty() {
            conflicts.push(Conflict {
                manager: other_manager.clone(),
                fields,
            });
        }
    }
    if !force && !conflicts.is_empty() {
        return Err(conflicts);
    }

    let mut result = managers.clone();
    for conflict in &conflicts {
        if let Some(fields) = result.get(&conflict.manager).cloned() {
            result.insert(
                conflict.manager.clone(),
                fields.difference(&conflict.fields),
            );
        }
    }
    for fields in result.values_mut() {
        *fields = fields.difference(&removed);
    }
    result.retain(|_, fields| !fields.is_empty());

    // SSA removes a value that this manager used to own when its new apply
    // configuration omits the field, unless another manager still owns it.
    let mut other_owned = Set::new();
    for (other_manager, fields) in &result {
        if other_manager != manager {
            other_owned = other_owned.union(fields);
        }
    }
    let to_remove = last_set.difference(&new_set).difference(&other_owned);
    let pruned = remove_items(schema, &merged, &to_remove);
    result.insert(manager.to_string(), new_set);

    let object = (pruned != *live).then_some(pruned);
    Ok(Applied {
        object,
        managers: result,
    })
}

/// Reconciles managed fields for an ordinary PUT/PATCH update. Unlike
/// [`apply`], an ordinary update never reports ownership conflicts: fields it
/// changed are transferred to the update manager, while fields it removed
/// are removed from every manager. This is the runtime-schema counterpart to
/// `patch::updater::apply_update` for CRD-defined resources.
pub fn apply_update(
    schema: &Value,
    live: &Value,
    new: &Value,
    managers: &BTreeMap<String, Set>,
    manager: &str,
) -> BTreeMap<String, Set> {
    let mut changed = Set::new();
    let mut removed = Set::new();
    diff(schema, live, new, &mut Vec::new(), &mut changed, &mut removed);

    let mut result = managers.clone();
    for (name, fields) in managers {
        if name != manager {
            result.insert(name.clone(), fields.difference(&changed));
        }
    }
    for fields in result.values_mut() {
        *fields = fields.difference(&removed);
    }

    let existing = managers.get(manager).cloned().unwrap_or_default();
    let fields = existing.difference(&removed).union(&changed);
    if fields.is_empty() {
        result.remove(manager);
    } else {
        result.insert(manager.to_string(), fields);
    }
    result.retain(|_, fields| !fields.is_empty());
    result
}

pub fn set_from_object(schema: &Value, value: &Value) -> Set {
    let mut set = Set::new();
    collect_object(schema, value, &mut Vec::new(), &mut set);
    set
}

/// Reconcile ownership recorded under an older version of a CRD schema with
/// the current schema. This is the runtime-schema counterpart to upstream's
/// `typed.ReconcileFieldSetWithSchema`: when a granular map or list becomes
/// atomic, ownership collapses to the containing field; the reverse change is
/// intentionally left as a shallow owner, matching upstream's behavior.
pub fn reconcile_field_set_with_schema(schema: &Value, fields: &Set) -> Set {
    reconcile_set(Some(schema), fields)
}

fn reconcile_set(schema: Option<&Value>, fields: &Set) -> Set {
    let mut elements = BTreeSet::new();
    elements.extend(fields.members.iter().cloned());
    elements.extend(fields.children.keys().cloned());

    let mut reconciled = Set::new();
    for element in elements {
        let child = fields.children.get(&element);
        let has_children = child.is_some_and(|child| !child.is_empty());
        let is_member = fields.members.contains(&element);
        let was_atomic = is_member && !has_children;
        let element_schema = schema.and_then(|schema| schema_for_element(schema, &element));

        if element_schema.is_some_and(is_atomic_schema) && !was_atomic {
            // Upstream treats a granular-to-atomic migration as ownership of
            // the whole field/list/map, dropping all children.
            reconciled.members.push(element);
            continue;
        }

        if is_member {
            reconciled.members.push(element.clone());
        }
        if let Some(child) = child {
            let child_schema = element_schema.and_then(schema_for_children);
            let child = reconcile_set(child_schema, child);
            if !child.is_empty() {
                reconciled.children.insert(element, child);
            }
        }
    }
    reconciled.members.sort();
    reconciled
}

fn schema_for_element<'a>(schema: &'a Value, element: &PathElement) -> Option<&'a Value> {
    match element {
        PathElement::Field(name) => field_schema(schema, name),
        PathElement::Key(_) | PathElement::Value(_) | PathElement::Index(_) => Some(schema),
    }
}

fn schema_for_children(schema: &Value) -> Option<&Value> {
    if schema.get("type").and_then(Value::as_str) == Some("array") {
        schema.get("items")
    } else {
        Some(schema)
    }
}

fn is_atomic_schema(schema: &Value) -> bool {
    schema.get("x-kubernetes-map-type").and_then(Value::as_str) == Some("atomic")
        || schema.get("x-kubernetes-list-type").and_then(Value::as_str) == Some("atomic")
}

/// Reconcile all stored CRD manager field sets against the current runtime
/// schema before an ordinary update or Apply compares ownership.
pub fn reconcile_managed_fields_with_schema(schema: &Value, managers: &BTreeMap<String, Set>) -> BTreeMap<String, Set> {
    managers
        .iter()
        .map(|(manager, fields)| (manager.clone(), reconcile_field_set_with_schema(schema, fields)))
        .collect()
}

fn collect_object(schema: &Value, value: &Value, path: &mut Vec<PathElement>, set: &mut Set) {
    let Some(object) = value.as_object() else {
        return;
    };
    for (key, value) in object {
        path.push(PathElement::Field(key.clone()));
        collect_field(field_schema(schema, key), value, path, set);
        path.pop();
    }
}

fn collect_field(
    schema: Option<&Value>,
    value: &Value,
    path: &mut Vec<PathElement>,
    set: &mut Set,
) {
    if schema.is_some_and(|schema| {
        schema.get("x-kubernetes-map-type").and_then(Value::as_str) == Some("atomic")
    }) && value.is_object()
    {
        set.insert(path);
        return;
    }
    match value {
        Value::Object(_) => collect_object(schema.unwrap_or(&Value::Null), value, path, set),
        Value::Array(values) => match schema.and_then(list_type) {
            Some("map") => {
                let keys = schema.and_then(list_map_keys).unwrap_or_default();
                let item_schema = schema.and_then(|schema| schema.get("items"));
                for value in values {
                    let Some(identity) = list_identity(value, &keys) else {
                        set.insert(path);
                        continue;
                    };
                    path.push(PathElement::Key(identity));
                    if value.is_object() {
                        collect_object(item_schema.unwrap_or(&Value::Null), value, path, set);
                    } else {
                        set.insert(path);
                    }
                    path.pop();
                }
            }
            Some("set") => {
                for value in values {
                    path.push(PathElement::Value(value.clone()));
                    set.insert(path);
                    path.pop();
                }
            }
            _ => set.insert(path),
        },
        _ => set.insert(path),
    }
}

fn merge(schema: &Value, live: &Value, config: &Value) -> Value {
    let (Some(live), Some(config)) = (live.as_object(), config.as_object()) else {
        return config.clone();
    };
    let mut merged = live.clone();
    for (key, config_value) in config {
        if config_value.is_null() {
            merged.remove(key);
            continue;
        }
        let field_schema = field_schema(schema, key);
        let value = match (merged.get(key), config_value) {
            (Some(_), Value::Object(_)) if field_schema.is_some_and(is_atomic_map) => config_value.clone(),
            (Some(live_value), Value::Object(_)) => merge(
                field_schema.unwrap_or(&Value::Null),
                live_value,
                config_value,
            ),
            (Some(Value::Array(live_values)), Value::Array(config_values))
                if field_schema.and_then(list_type) == Some("map") =>
            {
                Value::Array(merge_map_list(
                    field_schema.expect("list schema exists"),
                    live_values,
                    config_values,
                ))
            }
            (Some(Value::Array(live_values)), Value::Array(config_values))
                if field_schema.and_then(list_type) == Some("set") =>
            {
                let mut values = live_values.clone();
                for value in config_values {
                    if !values.contains(value) {
                        values.push(value.clone());
                    }
                }
                Value::Array(values)
            }
            _ => config_value.clone(),
        };
        merged.insert(key.clone(), value);
    }
    Value::Object(merged)
}

fn merge_map_list(schema: &Value, live: &[Value], config: &[Value]) -> Vec<Value> {
    let keys = list_map_keys(schema).unwrap_or_default();
    let item_schema = schema.get("items").unwrap_or(&Value::Null);
    let mut merged = live.to_vec();
    'next: for config_value in config {
        if let Some(identity) = list_identity(config_value, &keys) {
            for live_value in &mut merged {
                if list_identity(live_value, &keys).as_deref() == Some(identity.as_slice()) {
                    *live_value = merge(item_schema, live_value, config_value);
                    continue 'next;
                }
            }
        }
        merged.push(config_value.clone());
    }
    merged
}

fn diff(
    schema: &Value,
    old: &Value,
    new: &Value,
    path: &mut Vec<PathElement>,
    changed: &mut Set,
    removed: &mut Set,
) {
    if old == new {
        return;
    }
    let (Some(old_object), Some(new_object)) = (old.as_object(), new.as_object()) else {
        changed.insert(path);
        return;
    };

    let keys: BTreeSet<&str> = old_object
        .keys()
        .map(String::as_str)
        .chain(new_object.keys().map(String::as_str))
        .collect();
    for key in keys {
        path.push(PathElement::Field(key.to_string()));
        match (old_object.get(key), new_object.get(key)) {
            (Some(old_value), Some(new_value)) => diff_field(
                field_schema(schema, key),
                old_value,
                new_value,
                path,
                changed,
                removed,
            ),
            (None, Some(new_value)) => {
                collect_field(field_schema(schema, key), new_value, path, changed)
            }
            (Some(old_value), None) => {
                collect_field(field_schema(schema, key), old_value, path, removed)
            }
            (None, None) => {}
        }
        path.pop();
    }
}

fn diff_field(
    schema: Option<&Value>,
    old: &Value,
    new: &Value,
    path: &mut Vec<PathElement>,
    changed: &mut Set,
    removed: &mut Set,
) {
    if old == new {
        return;
    }
    if schema.is_some_and(is_atomic_map) {
        changed.insert(path);
        return;
    }
    if let (Some(old_values), Some(new_values)) = (old.as_array(), new.as_array()) {
        if schema.and_then(list_type) != Some("map") {
            changed.insert(path);
            return;
        }
        let keys = schema.and_then(list_map_keys).unwrap_or_default();
        let item_schema = schema
            .and_then(|schema| schema.get("items"))
            .unwrap_or(&Value::Null);
        for old_value in old_values {
            let Some(identity) = list_identity(old_value, &keys) else {
                removed.insert(path);
                continue;
            };
            match new_values
                .iter()
                .find(|value| list_identity(value, &keys).as_deref() == Some(identity.as_slice()))
            {
                Some(new_value) => {
                    path.push(PathElement::Key(identity));
                    diff(item_schema, old_value, new_value, path, changed, removed);
                    path.pop();
                }
                None => {
                    path.push(PathElement::Key(identity));
                    collect_object(item_schema, old_value, path, removed);
                    path.pop();
                }
            }
        }
        for new_value in new_values {
            let Some(identity) = list_identity(new_value, &keys) else {
                changed.insert(path);
                continue;
            };
            if !old_values
                .iter()
                .any(|value| list_identity(value, &keys).as_deref() == Some(identity.as_slice()))
            {
                path.push(PathElement::Key(identity));
                collect_object(item_schema, new_value, path, changed);
                path.pop();
            }
        }
        return;
    }
    if old.is_object() && new.is_object() {
        diff(
            schema.unwrap_or(&Value::Null),
            old,
            new,
            path,
            changed,
            removed,
        );
    } else {
        changed.insert(path);
    }
}

fn remove_items(schema: &Value, value: &Value, to_remove: &Set) -> Value {
    let Some(object) = value.as_object() else {
        return value.clone();
    };
    let mut result = object.clone();
    for key in object.keys() {
        let element = PathElement::Field(key.clone());
        if to_remove.members.contains(&element) {
            result.remove(key);
        } else if let Some(child) = to_remove.children.get(&element) {
            let next_schema = field_schema(schema, key).unwrap_or(&Value::Null);
            result.insert(
                key.clone(),
                remove_value(
                    next_schema,
                    object.get(key).expect("key came from object"),
                    child,
                ),
            );
        }
    }
    Value::Object(result)
}

fn remove_value(schema: &Value, value: &Value, to_remove: &Set) -> Value {
    match value {
        Value::Object(_) => remove_items(schema, value, to_remove),
        Value::Array(values) if list_type(schema) == Some("map") => {
            let keys = list_map_keys(schema).unwrap_or_default();
            let item_schema = schema.get("items").unwrap_or(&Value::Null);
            let mut result = Vec::new();
            for value in values {
                let Some(identity) = list_identity(value, &keys) else {
                    result.push(value.clone());
                    continue;
                };
                let element = PathElement::Key(identity);
                if to_remove.members.contains(&element) {
                    continue;
                }
                if let Some(child) = to_remove.children.get(&element) {
                    result.push(remove_value(item_schema, value, child));
                } else {
                    result.push(value.clone());
                }
            }
            Value::Array(result)
        }
        Value::Array(values) if list_type(schema) == Some("set") => Value::Array(
            values
                .iter()
                .filter(|value| {
                    !to_remove
                        .members
                        .contains(&PathElement::Value((*value).clone()))
                })
                .cloned()
                .collect(),
        ),
        _ => value.clone(),
    }
}

fn field_schema<'a>(schema: &'a Value, key: &str) -> Option<&'a Value> {
    schema
        .get("properties")
        .and_then(Value::as_object)
        .and_then(|properties| properties.get(key))
        .or_else(|| {
            schema
                .get("additionalProperties")
                .filter(|value| value.is_object())
        })
}

fn list_type(schema: &Value) -> Option<&str> {
    schema.get("x-kubernetes-list-type").and_then(Value::as_str)
}

fn is_atomic_map(schema: &Value) -> bool {
    schema.get("x-kubernetes-map-type").and_then(Value::as_str) == Some("atomic")
}

fn list_map_keys(schema: &Value) -> Option<Vec<&str>> {
    schema
        .get("x-kubernetes-list-map-keys")
        .and_then(Value::as_array)
        .map(|keys| keys.iter().filter_map(Value::as_str).collect())
}

fn list_identity(value: &Value, keys: &[&str]) -> Option<Vec<(String, Value)>> {
    let object = value.as_object()?;
    let mut identity = Vec::with_capacity(keys.len());
    for key in keys {
        identity.push(((*key).to_string(), object.get(*key)?.clone()));
    }
    identity.sort_by(|left, right| left.0.cmp(&right.0));
    Some(identity)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn schema() -> Value {
        json!({
            "type": "object",
            "properties": {
                "spec": {
                    "type": "object",
                    "properties": {
                        "color": {"type": "string"},
                        "ports": {"type": "array", "x-kubernetes-list-type": "map", "x-kubernetes-list-map-keys": ["name"], "items": {"type": "object", "properties": {"name": {"type": "string"}, "port": {"type": "integer"}}}},
                        "finalizers": {"type": "array", "x-kubernetes-list-type": "set", "items": {"type": "string"}}
                    }
                }
            }
        })
    }

    fn map_schema(map_type: Option<&str>) -> Value {
        let mut settings = json!({
            "type": "object",
            "properties": {
                "a": {"type": "string"},
                "b": {"type": "string"}
            }
        });
        if let Some(map_type) = map_type {
            settings["x-kubernetes-map-type"] = json!(map_type);
        }
        json!({
            "type": "object",
            "properties": {
                "spec": {
                    "type": "object",
                    "properties": {"settings": settings}
                }
            }
        })
    }

    #[test]
    fn different_managers_conflict_on_a_crd_field() {
        let schema = schema();
        let first = apply(
            &schema,
            &json!({}),
            &json!({"spec": {"color": "red"}}),
            &BTreeMap::new(),
            "one",
            false,
        )
        .unwrap();
        let second_managers = first.managers;
        let conflict = apply(
            &schema,
            &json!({"spec": {"color": "red"}}),
            &json!({"spec": {"color": "blue"}}),
            &second_managers,
            "two",
            false,
        )
        .unwrap_err();
        assert_eq!(conflict[0].manager, "one");
    }

    #[test]
    fn an_apply_can_remove_a_previously_owned_crd_field() {
        let schema = schema();
        let first = apply(
            &schema,
            &json!({}),
            &json!({"spec": {"color": "red", "ports": [{"name": "http", "port": 80}]}}),
            &BTreeMap::new(),
            "one",
            false,
        )
        .unwrap();
        let live = first.object.unwrap();
        let second = apply(
            &schema,
            &live,
            &json!({"spec": {"ports": [{"name": "http", "port": 80}]}}),
            &first.managers,
            "one",
            false,
        )
        .unwrap();
        assert_eq!(second.object.unwrap()["spec"].get("color"), None);
    }

    #[test]
    fn an_ordinary_update_transfers_changed_crd_fields_without_conflicting() {
        let schema = schema();
        let first = apply(
            &schema,
            &json!({}),
            &json!({"spec": {"color": "red", "ports": [{"name": "http", "port": 80}]}}),
            &BTreeMap::new(),
            "one",
            false,
        )
        .unwrap();
        let live = first.object.unwrap();
        let updated = apply_update(
            &schema,
            &live,
            &json!({"spec": {"color": "blue", "ports": [{"name": "http", "port": 80}]}}),
            &first.managers,
            "two",
        );

        assert!(!updated["one"].has(&[PathElement::Field("spec".to_string()), PathElement::Field("color".to_string())]));
        assert!(!updated["one"].is_empty(), "an unchanged CRD field must remain with its original manager");
        assert!(updated["two"].has(&[PathElement::Field("spec".to_string()), PathElement::Field("color".to_string())]));
    }

    #[test]
    fn granular_map_ownership_collapses_when_schema_becomes_atomic() {
        let granular = map_schema(None);
        let fields = set_from_object(&granular, &json!({"spec": {"settings": {"a": "one"}}}));
        let reconciled = reconcile_field_set_with_schema(&map_schema(Some("atomic")), &fields);

        assert!(reconciled.has(&[
            PathElement::Field("spec".to_string()),
            PathElement::Field("settings".to_string()),
        ]));
        assert!(!reconciled.has(&[
            PathElement::Field("spec".to_string()),
            PathElement::Field("settings".to_string()),
            PathElement::Field("a".to_string()),
        ]));
    }

    #[test]
    fn atomic_map_ownership_stays_shallow_when_schema_becomes_granular() {
        let atomic = map_schema(Some("atomic"));
        let fields = set_from_object(&atomic, &json!({"spec": {"settings": {"a": "one"}}}));
        let reconciled = reconcile_field_set_with_schema(&map_schema(None), &fields);

        assert!(reconciled.has(&[
            PathElement::Field("spec".to_string()),
            PathElement::Field("settings".to_string()),
        ]));
        assert!(!reconciled.has(&[
            PathElement::Field("spec".to_string()),
            PathElement::Field("settings".to_string()),
            PathElement::Field("a".to_string()),
        ]));
    }
}
