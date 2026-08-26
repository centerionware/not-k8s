use super::context::E2eContext;
use super::skip_test;
use anyhow::{Context, Result};
use k8s_openapi::api::core::v1::{PersistentVolumeClaim, Pod};
use kube::api::{Api, DeleteParams, PostParams};
use serde_json::json;
use std::time::Duration;

pub(super) async fn pod_mounts_a_generic_ephemeral_volume(context: &E2eContext) -> Result<()> {
    let storage_class = std::env::var("TEST_CSI_STORAGE_CLASS")
        .ok()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            skip_test(
                "TEST_CSI_STORAGE_CLASS is not set; a CSI-backed StorageClass is required",
            )
        })?;
    let pod_name = "ephemeral-vol-check";
    let claim_name = format!("{pod_name}-data");
    let pods: Api<Pod> = Api::namespaced(context.client.clone(), &context.namespace);
    let pvcs: Api<PersistentVolumeClaim> =
        Api::namespaced(context.client.clone(), &context.namespace);
    let pod: Pod = serde_json::from_value(json!({
        "apiVersion": "v1",
        "kind": "Pod",
        "metadata": {"name": pod_name},
        "spec": {
            "containers": [{
                "name": "app",
                "image": "busybox:latest",
                "command": ["sh", "-c", "echo hello-from-ephemeral-vol > /data/marker; sleep 3600"],
                "volumeMounts": [{"name": "data", "mountPath": "/data"}]
            }],
            "volumes": [{
                "name": "data",
                "ephemeral": {"volumeClaimTemplate": {
                    "metadata": {"labels": {"not-k8s-e2e": "generic-ephemeral"}},
                    "spec": {
                        "accessModes": ["ReadWriteOnce"],
                        "storageClassName": storage_class,
                        "resources": {"requests": {"storage": "64Mi"}}
                    }
                }}
            }]
        }
    }))?;
    pods.create(&PostParams::default(), &pod)
        .await
        .context("creating generic ephemeral-volume Pod")?;

    let result = async {
        context
            .wait_until(
                "generic ephemeral-volume controller to create the PVC",
                Duration::from_secs(90),
                || {
                    let pvcs = pvcs.clone();
                    let claim_name = claim_name.clone();
                    async move { Ok(pvcs.get_opt(&claim_name).await?.is_some()) }
                },
            )
            .await?;
        let pod_uid = pods
            .get(pod_name)
            .await?
            .metadata
            .uid
            .context("generic ephemeral-volume Pod has no UID")?;
        let claim = pvcs.get(&claim_name).await?;
        anyhow::ensure!(
            claim.metadata.owner_references.as_ref().is_some_and(|owners| {
                owners
                    .iter()
                    .any(|owner| owner.controller == Some(true) && owner.uid == pod_uid)
            }),
            "generic ephemeral PVC must be controller-owned by the Pod"
        );
        anyhow::ensure!(
            claim
                .metadata
                .labels
                .as_ref()
                .and_then(|labels| labels.get("not-k8s-e2e"))
                .is_some_and(|value| value == "generic-ephemeral"),
            "generic ephemeral PVC must preserve the volumeClaimTemplate label"
        );
        context
            .wait_until("generic ephemeral PVC Bound", Duration::from_secs(120), || {
                let pvcs = pvcs.clone();
                let claim_name = claim_name.clone();
                async move {
                    Ok(pvcs
                        .get(&claim_name)
                        .await?
                        .status
                        .and_then(|status| status.phase)
                        .as_deref()
                        == Some("Bound"))
                }
            })
            .await?;
        context
            .wait_until("generic ephemeral-volume Pod Running", Duration::from_secs(120), || {
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
    let _ = pvcs.delete(&claim_name, &DeleteParams::default()).await;
    result
}
