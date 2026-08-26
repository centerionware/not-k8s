use super::context::E2eContext;
use anyhow::{Context, Result};
use k8s_openapi::api::core::v1::Pod;
use k8s_openapi::api::node::v1::RuntimeClass;
use kube::api::{Api, DeleteParams, PostParams};
use serde_json::json;
use std::time::Duration;

pub(super) async fn runtime_class_handler_is_honored(context: &E2eContext) -> Result<()> {
    let handler = std::env::var("TEST_RUNTIME_CLASS_HANDLER").unwrap_or_else(|_| "runc".to_string());
    let class_name = format!("nodebootstrap-e2e-runtimeclass-{}", std::process::id());
    let pod_name = "runtimeclass-check";
    let runtime_classes: Api<RuntimeClass> = Api::all(context.client.clone());
    let pods: Api<Pod> = Api::namespaced(context.client.clone(), &context.namespace);
    let runtime_class: RuntimeClass = serde_json::from_value(json!({
        "apiVersion": "node.k8s.io/v1",
        "kind": "RuntimeClass",
        "metadata": {"name": class_name},
        "handler": handler
    }))?;
    runtime_classes
        .create(&PostParams::default(), &runtime_class)
        .await
        .context("creating RuntimeClass")?;

    let pod: Pod = serde_json::from_value(json!({
        "apiVersion": "v1",
        "kind": "Pod",
        "metadata": {"name": pod_name},
        "spec": {
            "runtimeClassName": class_name,
            "containers": [{"name": "app", "image": "busybox:latest", "command": ["sleep", "3600"]}]
        }
    }))?;
    let result = async {
        pods.create(&PostParams::default(), &pod)
            .await
            .context("creating RuntimeClass Pod")?;
        context
            .wait_until("RuntimeClass Pod Running", Duration::from_secs(90), || {
                let pods = pods.clone();
                async move {
                    Ok(pods
                        .get(pod_name)
                        .await?
                        .status
                        .and_then(|status| status.phase)
                        .as_deref()
                        == Some("Running"))
                }
            })
            .await
    }
    .await;

    let _ = pods.delete(pod_name, &DeleteParams::default()).await;
    let _ = runtime_classes
        .delete(&class_name, &DeleteParams::default())
        .await;
    result
}
