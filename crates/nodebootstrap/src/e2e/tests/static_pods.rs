use super::context::E2eContext;
use super::resource_managers::NodeletEnvOverride;
use super::skip_test;
use anyhow::{Context, Result};
use k8s_openapi::api::core::v1::Pod;
use kube::api::{Api, DeleteParams, ListParams};
use serde_json::json;
use std::fs;
use std::path::PathBuf;
use std::time::Duration;

struct StaticPodDir(PathBuf);

impl Drop for StaticPodDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

pub(super) async fn static_pod_creates_a_mirror_pod(context: &E2eContext) -> Result<()> {
    if crate::config::Config::from_env()?.nodelet_runtime() != "cri" {
        return Err(skip_test("static Pod checks require the CRI runtime"));
    }
    let path = std::env::temp_dir().join(format!(
        "nodebootstrap-static-pods-{}",
        std::process::id()
    ));
    fs::create_dir_all(&path)?;
    let _path_guard = StaticPodDir(path.clone());
    let _override = NodeletEnvOverride::install(&[(
        "NODELET_STATIC_POD_PATH",
        path.to_str().context("static Pod path is not UTF-8")?,
    )])?;
    let name = "static-e2e-check";
    let manifest = path.join(format!("{name}.json"));
    let pod: Pod = serde_json::from_value(json!({
        "apiVersion": "v1",
        "kind": "Pod",
        "metadata": {"name": name, "namespace": context.namespace},
        "spec": {"containers": [{"name": "app", "image": "busybox:latest", "command": ["sleep", "3600"]}]}
    }))?;
    fs::write(&manifest, serde_json::to_vec_pretty(&pod)?)?;
    let mirror = format!("{name}-{}", node_name(context).await?);
    let pods: Api<Pod> = Api::namespaced(context.client.clone(), &context.namespace);
    context
        .wait_until("static Pod mirror to reach Running", Duration::from_secs(120), || {
            let pods = pods.clone();
            async move {
                Ok(pods
                    .get(&mirror)
                    .await?
                    .status
                    .and_then(|status| status.phase)
                    .as_deref()
                    == Some("Running"))
            }
        })
        .await?;
    let mirror_pod = pods.get(&mirror).await?;
    let annotation = mirror_pod
        .metadata
        .annotations
        .unwrap_or_default()
        .get("kubernetes.io/config.mirror")
        .cloned()
        .context("static Pod mirror has no kubernetes.io/config.mirror annotation")?;
    anyhow::ensure!(!annotation.is_empty(), "static Pod mirror annotation is empty");
    fs::remove_file(&manifest)?;
    context
        .wait_until("static Pod mirror to disappear after manifest removal", Duration::from_secs(120), || {
            let pods = pods.clone();
            async move { Ok(pods.get_opt(&mirror).await?.is_none()) }
        })
        .await?;
    let _ = pods.delete(&mirror, &DeleteParams::default()).await;
    Ok(())
}

async fn node_name(context: &E2eContext) -> Result<String> {
    Api::<k8s_openapi::api::core::v1::Node>::all(context.client.clone())
        .list(&ListParams::default())
        .await?
        .items
        .into_iter()
        .find_map(|node| node.metadata.name)
        .context("cluster has no Node")
}
