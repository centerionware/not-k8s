//! `PodNodeSelector` admission for namespace node-selector annotations.
//!
//! Real upstream also accepts a cluster-wide selector configuration file.
//! This crate has no admission configuration-file surface yet, so this
//! module implements the independently useful and wire-visible part: the
//! legacy `scheduler.alpha.kubernetes.io/node-selector` annotation on a
//! Namespace. It is safe to run unconditionally because a Namespace must
//! explicitly opt into the behavior with that annotation.

use crate::admission::attributes::Operation;
use serde_json::Value;
use std::collections::BTreeMap;

pub const NODE_SELECTOR_ANNOTATION: &str = "scheduler.alpha.kubernetes.io/node-selector";

pub fn applies_to(operation: Operation, group: &str, resource: &str, subresource: &str) -> bool {
    operation == Operation::Create
        && group.is_empty()
        && resource == "pods"
        && subresource.is_empty()
}

/// Merge a Namespace's exact-match node selector into a Pod. The upstream
/// configuration parser uses a labels map, not the full label-selector
/// grammar: operators such as `In` and `NotIn` are rejected here as they are
/// by `labels.ConvertSelectorToLabelsMap`.
pub fn merge_namespace_selector(pod: &mut Value, selector: &str) -> Result<(), String> {
    let namespace_selector = parse_selector(selector)?;
    if namespace_selector.is_empty() {
        return Ok(());
    }

    let spec = pod
        .get_mut("spec")
        .and_then(Value::as_object_mut)
        .ok_or_else(|| "pod spec must be an object".to_string())?;
    let node_selector = spec
        .entry("nodeSelector")
        .or_insert_with(|| Value::Object(serde_json::Map::new()));
    let node_selector = node_selector
        .as_object_mut()
        .ok_or_else(|| "pod spec.nodeSelector must be an object".to_string())?;

    for (key, value) in namespace_selector {
        if let Some(existing) = node_selector.get(&key).and_then(Value::as_str) {
            if existing != value {
                return Err(
                    "pod node label selector conflicts with its namespace node label selector"
                        .to_string(),
                );
            }
        } else if node_selector.contains_key(&key) {
            return Err(format!("pod spec.nodeSelector[{key:?}] must be a string"));
        } else {
            node_selector.insert(key, Value::String(value));
        }
    }
    Ok(())
}

fn parse_selector(selector: &str) -> Result<BTreeMap<String, String>, String> {
    let mut result = BTreeMap::new();
    if selector.trim().is_empty() {
        return Ok(result);
    }
    for term in selector.split(',') {
        let term = term.trim();
        if term.is_empty() {
            return Err("namespace node selector contains an empty requirement".to_string());
        }
        let Some((key, value)) = term.split_once('=') else {
            return Err(format!(
                "namespace node selector {term:?} is not an exact key=value selector"
            ));
        };
        let key = key.trim();
        let value = value.trim();
        if key.is_empty() || value.is_empty() {
            return Err(format!(
                "namespace node selector {term:?} is not an exact key=value selector"
            ));
        }
        if result.insert(key.to_string(), value.to_string()).is_some() {
            return Err(format!(
                "namespace node selector contains duplicate key {key:?}"
            ));
        }
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn namespace_selector_is_merged_into_a_pod() {
        let mut pod = json!({"spec": {"nodeSelector": {"disk": "ssd"}}});
        merge_namespace_selector(&mut pod, "zone=blue, arch=arm64").unwrap();
        assert_eq!(
            pod["spec"]["nodeSelector"],
            json!({"arch": "arm64", "disk": "ssd", "zone": "blue"})
        );
    }

    #[test]
    fn conflicting_selector_is_rejected() {
        let mut pod = json!({"spec": {"nodeSelector": {"zone": "red"}}});
        let error = merge_namespace_selector(&mut pod, "zone=blue").unwrap_err();
        assert!(error.contains("conflicts"));
    }

    #[test]
    fn non_exact_selector_syntax_is_rejected() {
        let mut pod = json!({"spec": {}});
        assert!(merge_namespace_selector(&mut pod, "zone in (blue,red)").is_err());
    }

    #[test]
    fn no_annotation_value_is_a_noop() {
        let mut pod = json!({"spec": {}});
        merge_namespace_selector(&mut pod, "").unwrap();
        assert_eq!(pod, json!({"spec": {}}));
    }
}
