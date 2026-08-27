//! Small synchronous bridge for nodebootstrap's install-time Kubernetes API calls.
//!
//! The bootstrapper is intentionally a synchronous, one-shot CLI, while the
//! kube client is async. Keep the runtime construction and kubeconfig loading
//! in one place so install-time resource operations use the library client
//! directly instead of depending on a `kubectl` executable.

use anyhow::{Context, Result};
use kube::api::{Api, DynamicObject, Patch, PatchParams};
use kube::config::Kubeconfig;
use kube::core::{GroupVersionKind, ResourceExt};
use kube::discovery::ApiResource;
use kube::Client;
use serde::Deserialize;
use std::future::Future;
use std::path::Path;

pub fn block_on<T, F, Fut>(kubeconfig_path: &Path, operation: F) -> Result<T>
where
    F: FnOnce(Client) -> Fut,
    Fut: Future<Output = Result<T>>,
{
    let kubeconfig = Kubeconfig::read_from(kubeconfig_path)
        .with_context(|| format!("reading {} for the Kubernetes API", kubeconfig_path.display()))?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("building the nodebootstrap Kubernetes API runtime")?;

    runtime.block_on(async move {
        let client = Client::try_from(kubeconfig).context("building the Kubernetes API client")?;
        operation(client).await
    })
}

/// Apply every object in a Kubernetes multi-document YAML manifest through
/// server-side apply. The manifest sources used by bootstrap are all concrete
/// resources, so their API resource plural can safely be derived from kind.
pub async fn apply_yaml(client: &Client, manifest: &str, field_manager: &str) -> Result<usize> {
    let objects = decode_manifest(manifest)?;
    let mut applied = 0;
    for object in objects {
        let type_meta = object
            .types
            .as_ref()
            .context("manifest document has no apiVersion/kind")?;
        let gvk = GroupVersionKind::try_from(type_meta).context("parsing manifest apiVersion")?;
        let resource = ApiResource::from_gvk(&gvk);
        let name = object.name_any();
        let api: Api<DynamicObject> = match object.namespace() {
            Some(namespace) => Api::namespaced_with(client.clone(), &namespace, &resource),
            None => Api::all_with(client.clone(), &resource),
        };
        api.patch(
            &name,
            &PatchParams::apply(field_manager),
            &Patch::Apply(&object),
        )
        .await
        .with_context(|| {
            format!(
                "applying {}/{} {}",
                object.types.as_ref().map(|types| types.api_version.as_str()).unwrap_or("unknown"),
                object.types.as_ref().map(|types| types.kind.as_str()).unwrap_or("unknown"),
                name
            )
        })?;
        applied += 1;
    }
    Ok(applied)
}

fn decode_manifest(manifest: &str) -> Result<Vec<DynamicObject>> {
    let mut objects = Vec::new();
    for (index, document) in serde_yaml::Deserializer::from_str(manifest).enumerate() {
        let value = serde_yaml::Value::deserialize(document)
            .with_context(|| format!("parsing Kubernetes manifest document {index}"))?;
        if value.is_null() {
            continue;
        }
        let object: DynamicObject = serde_yaml::from_value(value)
            .with_context(|| format!("decoding Kubernetes manifest document {index}"))?;
        objects.push(object);
    }
    Ok(objects)
}

#[cfg(test)]
mod tests {
    use super::decode_manifest;
    use kube::ResourceExt;

    #[test]
    fn decodes_multi_document_manifests() {
        let objects = decode_manifest(
            "---\napiVersion: v1\nkind: ConfigMap\nmetadata:\n  name: one\n  namespace: default\n---\napiVersion: v1\nkind: Service\nmetadata:\n  name: two\n  namespace: default\n",
        )
        .expect("decode manifest");
        assert_eq!(objects.len(), 2);
        assert_eq!(objects[0].name_any(), "one");
        assert_eq!(objects[1].name_any(), "two");
    }
}
