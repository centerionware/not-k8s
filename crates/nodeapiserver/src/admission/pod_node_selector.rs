//! `PodNodeSelector` admission for namespace node-selector annotations.
//!
//! Real upstream also accepts a cluster-wide selector configuration file.
//! Both that configuration and the legacy
//! `scheduler.alpha.kubernetes.io/node-selector` Namespace annotation are
//! supported here. The annotation takes precedence over the configured
//! namespace or cluster default, matching the upstream plugin's layering.

use crate::admission::attributes::Operation;
use serde::Deserialize;
use serde_json::Value;
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

pub const NODE_SELECTOR_ANNOTATION: &str = "scheduler.alpha.kubernetes.io/node-selector";

/// The file-shaped part of upstream's `PodNodeSelector` configuration.
/// Unknown top-level keys are namespace names and map to exact-match label
/// selectors; `clusterDefaultNodeSelector` applies when a namespace has no
/// explicit entry.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
pub struct PluginConfig {
    #[serde(default, rename = "clusterDefaultNodeSelector")]
    pub cluster_default_node_selector: String,
    #[serde(flatten)]
    pub namespace_selectors: BTreeMap<String, String>,
}

impl PluginConfig {
    /// Read and validate one plugin configuration file at startup. Parsing
    /// selectors here makes a malformed operator configuration fail before it
    /// can affect only some later Pod requests.
    pub fn from_file(path: &Path) -> Result<Self, String> {
        let contents = fs::read_to_string(path)
            .map_err(|error| format!("reading PodNodeSelector config {}: {error}", path.display()))?;
        let config: Self = serde_yaml::from_str(&contents)
            .map_err(|error| format!("decoding PodNodeSelector config {}: {error}", path.display()))?;
        validate_selector(&config.cluster_default_node_selector, "clusterDefaultNodeSelector")?;
        for (namespace, selector) in &config.namespace_selectors {
            validate_selector(selector, namespace)?;
        }
        Ok(config)
    }

    fn selector_for_namespace(&self, namespace: &str) -> &str {
        self.namespace_selectors
            .get(namespace)
            .map(String::as_str)
            .unwrap_or(&self.cluster_default_node_selector)
    }
}

/// Select the effective selector for one Pod. A non-empty Namespace
/// annotation overrides the file; absent file and annotation produce the
/// empty selector and therefore preserve the existing no-op behavior.
pub fn selector_for_namespace(
    config: Option<&PluginConfig>,
    namespace: &str,
    annotation: Option<&str>,
) -> Result<String, String> {
    let selector = annotation
        .filter(|selector| !selector.trim().is_empty())
        .or_else(|| config.map(|config| config.selector_for_namespace(namespace)))
        .unwrap_or("");
    validate_selector(selector, "PodNodeSelector")?;
    Ok(selector.to_string())
}

fn validate_selector(selector: &str, source: &str) -> Result<(), String> {
    parse_selector(selector)
        .map(|_| ())
        .map_err(|error| format!("{source}: {error}"))
}

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

    #[test]
    fn namespace_config_overrides_the_cluster_default() {
        let config = PluginConfig {
            cluster_default_node_selector: "environment=production".to_string(),
            namespace_selectors: BTreeMap::from([("development".to_string(), "environment=test".to_string())]),
        };
        assert_eq!(selector_for_namespace(Some(&config), "development", None).unwrap(), "environment=test");
        assert_eq!(selector_for_namespace(Some(&config), "other", None).unwrap(), "environment=production");
    }

    #[test]
    fn an_explicit_empty_namespace_selector_disables_the_cluster_default() {
        let config = PluginConfig {
            cluster_default_node_selector: "environment=production".to_string(),
            namespace_selectors: BTreeMap::from([("development".to_string(), String::new())]),
        };
        assert_eq!(selector_for_namespace(Some(&config), "development", None).unwrap(), "");
    }

    #[test]
    fn namespace_annotation_overrides_file_configuration() {
        let config = PluginConfig {
            cluster_default_node_selector: "environment=production".to_string(),
            namespace_selectors: BTreeMap::new(),
        };
        assert_eq!(
            selector_for_namespace(Some(&config), "default", Some("environment=development")).unwrap(),
            "environment=development"
        );
    }

    #[test]
    fn malformed_file_selector_is_rejected() {
        let config = PluginConfig {
            cluster_default_node_selector: "environment in (production,test)".to_string(),
            namespace_selectors: BTreeMap::new(),
        };
        let error = selector_for_namespace(Some(&config), "default", None).unwrap_err();
        assert!(error.contains("not an exact key=value selector"));
    }

    #[test]
    fn yaml_file_is_decoded_with_namespace_keys() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("pod-node-selector.yaml");
        std::fs::write(
            &path,
            "clusterDefaultNodeSelector: environment=production\ndevelopment: environment=test\n",
        )
        .unwrap();
        let config = PluginConfig::from_file(&path).unwrap();
        assert_eq!(config.selector_for_namespace("development"), "environment=test");
        assert_eq!(config.selector_for_namespace("other"), "environment=production");
    }
}
