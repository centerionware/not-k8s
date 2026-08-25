use super::context::E2eContext;
use super::skip_test;
use anyhow::{Context, Result};
use k8s_openapi::api::core::v1::{PersistentVolumeClaim, Pod};
use kube::api::{Api, DeleteParams, PostParams};
use serde_json::json;
use std::path::Path;
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

pub(super) async fn pod_uses_a_raw_block_volume(context: &E2eContext) -> Result<()> {
    if crate::config::Config::from_env()?.nodelet_runtime() != "cri" {
        return Err(skip_test("raw block-volume checks require the CRI runtime"));
    }
    let storage_class = std::env::var("TEST_CSI_BLOCK_STORAGE_CLASS")
        .ok()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            skip_test(
                "TEST_CSI_BLOCK_STORAGE_CLASS is not set; a CSI StorageClass supporting volumeMode: Block is required",
            )
        })?;
    let pod_name = "csi-block-check";
    let claim_name = "csi-block-check-claim";
    let pods: Api<Pod> = Api::namespaced(context.client.clone(), &context.namespace);
    let pvcs: Api<PersistentVolumeClaim> =
        Api::namespaced(context.client.clone(), &context.namespace);
    let claim: PersistentVolumeClaim = serde_json::from_value(json!({
        "apiVersion": "v1",
        "kind": "PersistentVolumeClaim",
        "metadata": {"name": claim_name},
        "spec": {
            "accessModes": ["ReadWriteOnce"],
            "volumeMode": "Block",
            "storageClassName": storage_class,
            "resources": {"requests": {"storage": "64Mi"}}
        }
    }))?;
    pvcs.create(&PostParams::default(), &claim)
        .await
        .context("creating raw block-volume PVC")?;

    let bind_result = context
        .wait_until("raw block-volume PVC to become Bound", Duration::from_secs(120), || {
            let pvcs = pvcs.clone();
            async move {
                Ok(pvcs
                    .get(claim_name)
                    .await?
                    .status
                    .and_then(|status| status.phase)
                    .as_deref()
                    == Some("Bound"))
            }
        })
        .await;
    if let Err(error) = bind_result {
        let _ = pvcs.delete(claim_name, &DeleteParams::default()).await;
        return Err(error);
    }

    let pod: Pod = serde_json::from_value(json!({
        "apiVersion": "v1",
        "kind": "Pod",
        "metadata": {"name": pod_name},
        "spec": {
            "containers": [{
                "name": "app",
                "image": "busybox:latest",
                "command": ["sleep", "3600"],
                "volumeDevices": [{"name": "raw", "devicePath": "/dev/xvda"}]
            }],
            "volumes": [{
                "name": "raw",
                "persistentVolumeClaim": {"claimName": claim_name}
            }]
        }
    }))?;
    pods.create(&PostParams::default(), &pod)
        .await
        .context("creating raw block-volume Pod")?;

    let result = async {
        context
            .wait_until("raw block-volume Pod to reach Running", Duration::from_secs(120), || {
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
            .await?;
        let pod_uid = pods
            .get(pod_name)
            .await?
            .metadata
            .uid
            .context("raw block-volume Pod has no UID")?;
        let disk_path = std::env::var("NODELET_DISK_PATH")
            .unwrap_or_else(|_| "/var/lib/nodelet".to_string());
        let target_path = format!("{disk_path}/pods/{pod_uid}/volumes/raw");
        context
            .wait_until("raw block-volume target to appear", Duration::from_secs(30), || {
                let target_path = target_path.clone();
                async move { Ok(Path::new(&target_path).exists()) }
            })
            .await?;
        anyhow::ensure!(
            !Path::new(&target_path).is_dir(),
            "raw block-volume target {target_path} must be a file or device, not a directory"
        );
        Ok(())
    }
    .await;

    let _ = pods.delete(pod_name, &DeleteParams::default()).await;
    let _ = pvcs.delete(claim_name, &DeleteParams::default()).await;
    result
}
