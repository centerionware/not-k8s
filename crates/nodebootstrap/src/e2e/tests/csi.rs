use super::context::E2eContext;
use super::skip_test;
use anyhow::Result;
use k8s_openapi::api::core::v1::Pod;
use kube::api::{Api, PostParams};
use serde_json::json;
use std::time::Duration;

pub(super) async fn csi_ephemeral_inline_volume_is_mounted(
    context: &E2eContext,
) -> Result<()> {
    let driver = std::env::var("TEST_CSI_INLINE_DRIVER")
        .ok()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            skip_test(
                "TEST_CSI_INLINE_DRIVER is not set; an inline-capable CSI driver is required",
            )
        })?;
    let name = "csi-ephemeral-inline";
    let pods: Api<Pod> = Api::namespaced(context.client.clone(), &context.namespace);
    let pod: Pod = serde_json::from_value(json!({
        "apiVersion": "v1",
        "kind": "Pod",
        "metadata": {"name": name},
        "spec": {
            "containers": [{
                "name": "app",
                "image": "busybox:latest",
                "command": ["sh", "-c", "test -d /data && sleep 3600"],
                "volumeMounts": [{"name": "data", "mountPath": "/data", "readOnly": true}]
            }],
            "volumes": [{
                "name": "data",
                "csi": {"driver": driver, "readOnly": true}
            }]
        }
    }))?;
    pods.create(&PostParams::default(), &pod).await?;
    context
        .wait_until("CSI ephemeral inline-volume Pod Running", Duration::from_secs(120), || {
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
        .await
}
