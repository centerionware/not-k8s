//! Shared loader for Kubernetes webhook kubeconfig files.
//!
//! Audit and authorization webhooks use the same kubeconfig-shaped transport
//! configuration: the selected cluster supplies the endpoint and optional CA,
//! while the selected user supplies optional client certificate credentials.

use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug)]
pub(crate) struct WebhookConfig {
    pub(crate) url: String,
    pub(crate) ca_pem: Option<Vec<u8>>,
    pub(crate) identity_pem: Option<Vec<u8>>,
}

pub(crate) fn load_kubeconfig(path: &Path) -> Result<WebhookConfig, String> {
    let yaml = fs::read_to_string(path)
        .map_err(|error| format!("reading webhook config {}: {error}", path.display()))?;
    let document: Value = serde_yaml::from_str(&yaml)
        .map_err(|error| format!("decoding webhook config {}: {error}", path.display()))?;
    let context_name = document
        .get("current-context")
        .and_then(Value::as_str)
        .filter(|name| !name.is_empty())
        .ok_or_else(|| "webhook config has no current-context".to_string())?;
    let context = named_section(&document, "contexts", context_name, "context")?;
    let cluster_name = required_string(context, "cluster", "context.cluster")?;
    let cluster = named_section(&document, "clusters", cluster_name, "cluster")?;
    let url = required_string(cluster, "server", "cluster.server")?.to_string();
    let base = path.parent().unwrap_or_else(|| Path::new("."));
    let ca_pem = data_or_file(cluster, "certificate-authority-data", "certificate-authority", base)?;

    let identity_pem = match context.get("user").and_then(Value::as_str) {
        None | Some("") => None,
        Some(user_name) => {
            let user = named_section(&document, "users", user_name, "user")?;
            let certificate = data_or_file(user, "client-certificate-data", "client-certificate", base)?;
            let key = data_or_file(user, "client-key-data", "client-key", base)?;
            match (certificate, key) {
                (None, None) => None,
                (Some(_), None) | (None, Some(_)) => {
                    return Err("webhook user must provide both client certificate and client key".to_string());
                }
                (Some(certificate), Some(key)) => {
                    let mut identity = certificate;
                    identity.push(b'\n');
                    identity.extend_from_slice(&key);
                    Some(identity)
                }
            }
        }
    };

    Ok(WebhookConfig { url, ca_pem, identity_pem })
}

fn named_section<'a>(document: &'a Value, list_name: &str, name: &str, section_name: &str) -> Result<&'a Value, String> {
    document
        .get(list_name)
        .and_then(Value::as_array)
        .and_then(|entries| entries.iter().find(|entry| entry.get("name").and_then(Value::as_str) == Some(name)))
        .and_then(|entry| entry.get(section_name))
        .ok_or_else(|| format!("webhook config has no {section_name} entry named {name:?}"))
}

fn required_string<'a>(value: &'a Value, key: &str, description: &str) -> Result<&'a str, String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| format!("webhook config is missing {description}"))
}

fn data_or_file(value: &Value, data_key: &str, file_key: &str, base: &Path) -> Result<Option<Vec<u8>>, String> {
    if let Some(encoded) = value.get(data_key).and_then(Value::as_str).filter(|value| !value.is_empty()) {
        use base64::Engine;
        return base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .map(Some)
            .map_err(|error| format!("webhook config field {data_key} is not valid base64: {error}"));
    }
    let Some(file) = value.get(file_key).and_then(Value::as_str).filter(|value| !value.is_empty()) else {
        return Ok(None);
    };
    let file = PathBuf::from(file);
    let file = if file.is_absolute() { file } else { base.join(file) };
    fs::read(&file)
        .map(Some)
        .map_err(|error| format!("reading webhook credential {}: {error}", file.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;

    #[test]
    fn loads_the_selected_kubeconfig_cluster_and_user() {
        let path = std::env::temp_dir().join(format!("nodeapiserver-webhook-{}.yaml", std::process::id()));
        std::fs::write(
            &path,
            "apiVersion: v1\nkind: Config\ncurrent-context: audit\nclusters:\n- name: backend\n  cluster:\n    server: https://webhook.example/events\nusers:\n- name: sender\n  user: {}\ncontexts:\n- name: audit\n  context:\n    cluster: backend\n    user: sender\n",
        )
        .unwrap();
        let config = load_kubeconfig(&path).unwrap();
        assert_eq!(config.url, "https://webhook.example/events");
        assert!(config.ca_pem.is_none());
        assert!(config.identity_pem.is_none());
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn resolves_relative_credentials_and_prefers_inline_data() {
        let directory = std::env::temp_dir().join(format!("nodeapiserver-webhook-dir-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(directory.join("ca.pem"), b"file-ca").unwrap();
        let path = directory.join("config.yaml");
        let inline_ca = base64::engine::general_purpose::STANDARD.encode(b"inline-ca");
        std::fs::write(
            &path,
            format!(
                "apiVersion: v1\nkind: Config\ncurrent-context: audit\nclusters:\n- name: backend\n  cluster:\n    server: https://webhook.example/events\n    certificate-authority-data: {inline_ca}\n    certificate-authority: ca.pem\nusers:\n- name: sender\n  user: {{}}\ncontexts:\n- name: audit\n  context:\n    cluster: backend\n    user: sender\n"
            ),
        )
        .unwrap();
        let config = load_kubeconfig(&path).unwrap();
        assert_eq!(config.ca_pem.as_deref(), Some(b"inline-ca".as_slice()));
        std::fs::remove_file(path).unwrap();
        std::fs::remove_file(directory.join("ca.pem")).unwrap();
        std::fs::remove_dir(directory).unwrap();
    }
}
