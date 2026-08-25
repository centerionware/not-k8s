use super::context::E2eContext;
use super::skip_test;
use anyhow::Result;
use k8s_openapi::api::core::v1::Pod;
use kube::api::{Api, PostParams};
use serde_json::json;
use std::time::Duration;

fn needs_cri() -> Result<()> {
    anyhow::ensure!(
        crate::config::Config::from_env()?.nodelet_runtime() == "cri",
        "device-resource status checks require the CRI runtime",
    );
    Ok(())
}

pub(super) async fn plugin_registry_watches_for_device_plugins_too(
    _context: &E2eContext,
) -> Result<()> {
    if let Err(error) = needs_cri() {
        return Err(skip_test(error.to_string()));
    }
    let path = std::env::var("NODELET_PLUGIN_REGISTRY_PATH")
        .unwrap_or_else(|_| "/var/lib/nodelet/plugins_registry".to_owned());
    if !std::path::Path::new(&path).is_dir() {
        return Err(skip_test(format!(
            "plugin registry directory {path} is not present on this deployment"
        )));
    }
    Ok(())
}

pub(super) async fn allocated_resources_status_absent_without_device_resources(
    context: &E2eContext,
) -> Result<()> {
    if let Err(error) = needs_cri() {
        return Err(skip_test(error.to_string()));
    }
    let name = "no-device-resources";
    let pods: Api<Pod> = Api::namespaced(context.client.clone(), &context.namespace);
    let pod: Pod = serde_json::from_value(json!({
        "apiVersion": "v1",
        "kind": "Pod",
        "metadata": {"name": name},
        "spec": {
            "containers": [{"name": "app", "image": "busybox:latest", "command": ["sleep", "3600"]}]
        }
    }))?;
    pods.create(&PostParams::default(), &pod).await?;
    context
        .wait_until("plain Pod to reach Running without device resources", Duration::from_secs(90), || {
            let pods = pods.clone();
            async move {
                Ok(pods
                    .get(name)
                    .await?
                    .status
                    .and_then(|status| status.phase)
                    .as_deref()
                    == Some("Running"))
            }
        })
        .await?;
    let status = serde_json::to_value(pods.get(name).await?)?;
    let allocated = status.pointer("/status/containerStatuses/0/allocatedResourcesStatus");
    let has_allocated_resources = allocated.is_some_and(|value| {
        !value.is_null() && !value.as_object().is_some_and(|object| object.is_empty())
    });
    anyhow::ensure!(
        !has_allocated_resources,
        "a Pod without device-plugin resources unexpectedly reported allocatedResourcesStatus: {allocated:?}"
    );
    Ok(())
}
