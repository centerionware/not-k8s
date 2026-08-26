use super::context::E2eContext;
use super::skip_test;
use anyhow::{Context, Result};
use k8s_openapi::api::core::v1::{Node, PersistentVolume, PersistentVolumeClaim, Pod};
use k8s_openapi::api::storage::v1::VolumeAttachment;
use kube::api::{Api, DeleteParams, ListParams, PostParams};
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

pub(super) async fn node_reports_volumes_in_use_for_a_csi_volume(
    context: &E2eContext,
) -> Result<()> {
    if crate::config::Config::from_env()?.nodelet_runtime() != "cri" {
        return Err(skip_test("CSI volumesInUse checks require the CRI runtime"));
    }
    let storage_class = std::env::var("TEST_CSI_STORAGE_CLASS")
        .ok()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            skip_test("TEST_CSI_STORAGE_CLASS is not set; a CSI-backed StorageClass is required")
        })?;
    let pod_name = "volumes-in-use-check";
    let claim_name = "volumes-in-use-check-claim";
    let nodes: Api<Node> = Api::all(context.client.clone());
    let node_name = nodes
        .list(&ListParams::default())
        .await?
        .items
        .into_iter()
        .find_map(|node| node.metadata.name)
        .context("the cluster has no Node object")?;
    let pods: Api<Pod> = Api::namespaced(context.client.clone(), &context.namespace);
    let pvcs: Api<PersistentVolumeClaim> =
        Api::namespaced(context.client.clone(), &context.namespace);
    let pvs: Api<PersistentVolume> = Api::all(context.client.clone());
    let claim: PersistentVolumeClaim = serde_json::from_value(json!({
        "apiVersion": "v1",
        "kind": "PersistentVolumeClaim",
        "metadata": {"name": claim_name},
        "spec": {
            "accessModes": ["ReadWriteOnce"],
            "storageClassName": storage_class,
            "resources": {"requests": {"storage": "64Mi"}}
        }
    }))?;
    pvcs.create(&PostParams::default(), &claim).await?;
    let bind_result = context
        .wait_until("CSI volumesInUse PVC to become Bound", Duration::from_secs(120), || {
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
            "containers": [{"name": "app", "image": "busybox:latest", "command": ["sleep", "3600"], "volumeMounts": [{"name": "data", "mountPath": "/data"}]}],
            "volumes": [{"name": "data", "persistentVolumeClaim": {"claimName": claim_name}}]
        }
    }))?;
    pods.create(&PostParams::default(), &pod).await?;

    let result = async {
        context
            .wait_until("CSI volumesInUse Pod Running", Duration::from_secs(120), || {
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
        let pv_name = serde_json::to_value(pvcs.get(claim_name).await?)?
            .pointer("/spec/volumeName")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
            .context("bound CSI PVC has no spec.volumeName")?;
        let pv = serde_json::to_value(pvs.get(&pv_name).await?)?;
        let driver = pv
            .pointer("/spec/csi/driver")
            .and_then(serde_json::Value::as_str)
            .context("bound PV has no CSI driver")?;
        let handle = pv
            .pointer("/spec/csi/volumeHandle")
            .and_then(serde_json::Value::as_str)
            .context("bound PV has no CSI volumeHandle")?;
        let expected = format!("kubernetes.io/csi/{driver}^{handle}");
        context
            .wait_until("Node.status.volumesInUse CSI entry", Duration::from_secs(150), || {
                let nodes = nodes.clone();
                let node_name = node_name.clone();
                let expected = expected.clone();
                async move {
                    let value = serde_json::to_value(nodes.get(&node_name).await?)?;
                    Ok(value
                        .pointer("/status/volumesInUse")
                        .and_then(serde_json::Value::as_array)
                        .is_some_and(|entries| {
                            entries.iter().any(|entry| entry.as_str() == Some(expected.as_str()))
                        }))
                }
            })
            .await?;
        pods.delete(pod_name, &DeleteParams::default()).await?;
        context
            .wait_until("CSI volumesInUse Pod deletion", Duration::from_secs(120), || {
                let pods = pods.clone();
                async move { Ok(pods.get_opt(pod_name).await?.is_none()) }
            })
            .await?;
        context
            .wait_until("Node.status.volumesInUse CSI entry cleared", Duration::from_secs(150), || {
                let nodes = nodes.clone();
                let node_name = node_name.clone();
                let expected = expected.clone();
                async move {
                    let value = serde_json::to_value(nodes.get(&node_name).await?)?;
                    Ok(!value
                        .pointer("/status/volumesInUse")
                        .and_then(serde_json::Value::as_array)
                        .is_some_and(|entries| {
                            entries.iter().any(|entry| entry.as_str() == Some(expected.as_str()))
                        }))
                }
            })
            .await
    }
    .await;
    let _ = pods.delete(pod_name, &DeleteParams::default()).await;
    let _ = pvcs.delete(claim_name, &DeleteParams::default()).await;
    result
}

async fn pod_termination_message(context: &E2eContext, name: &str) -> Result<Option<String>> {
    let pods: Api<Pod> = Api::namespaced(context.client.clone(), &context.namespace);
    Ok(pods
        .get(name)
        .await?
        .status
        .and_then(|status| status.container_statuses)
        .unwrap_or_default()
        .into_iter()
        .find(|status| status.name == "app")
        .and_then(|status| status.state)
        .and_then(|state| state.terminated)
        .and_then(|terminated| terminated.message))
}

pub(super) async fn fsgroup_change_policy_on_root_mismatch_skips_the_second_chown(
    context: &E2eContext,
) -> Result<()> {
    if crate::config::Config::from_env()?.nodelet_runtime() != "cri" {
        return Err(skip_test("CSI fsGroupChangePolicy checks require the CRI runtime"));
    }
    let storage_class = std::env::var("TEST_CSI_STORAGE_CLASS")
        .ok()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            skip_test("TEST_CSI_STORAGE_CLASS is not set; a CSI-backed StorageClass is required")
        })?;
    let claim_name = "fsgroup-policy-check-claim";
    let first_name = "fsgroup-policy-check-1";
    let second_name = "fsgroup-policy-check-2";
    let pods: Api<Pod> = Api::namespaced(context.client.clone(), &context.namespace);
    let pvcs: Api<PersistentVolumeClaim> =
        Api::namespaced(context.client.clone(), &context.namespace);
    let attachments: Api<VolumeAttachment> = Api::all(context.client.clone());
    let claim: PersistentVolumeClaim = serde_json::from_value(json!({
        "apiVersion": "v1",
        "kind": "PersistentVolumeClaim",
        "metadata": {"name": claim_name},
        "spec": {
            "accessModes": ["ReadWriteOnce"],
            "storageClassName": storage_class,
            "resources": {"requests": {"storage": "64Mi"}}
        }
    }))?;
    pvcs.create(&PostParams::default(), &claim).await?;
    let bind_result = context
        .wait_until("fsGroup policy PVC to become Bound", Duration::from_secs(120), || {
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

    let create_pod = |name: &'static str| {
        serde_json::from_value::<Pod>(json!({
            "apiVersion": "v1",
            "kind": "Pod",
            "metadata": {"name": name},
            "spec": {
                "restartPolicy": "Never",
                "securityContext": {"fsGroup": 4322, "fsGroupChangePolicy": "OnRootMismatch"},
                "containers": [{"name": "app", "image": "busybox:latest", "command": ["sh", "-c", "stat -c %g /data > /dev/termination-log"], "volumeMounts": [{"name": "data", "mountPath": "/data"}]}],
                "volumes": [{"name": "data", "persistentVolumeClaim": {"claimName": claim_name}}]
            }
        }))
    };
    let first: Pod = create_pod(first_name)?;
    pods.create(&PostParams::default(), &first).await?;
    let first_result = context
        .wait_until("first fsGroup policy Pod", Duration::from_secs(120), || {
            let context = context.clone();
            async move {
                Ok(pod_termination_message(&context, first_name)
                    .await?
                    .is_some_and(|message| message.trim() == "4322"))
            }
        })
        .await;
    if let Err(error) = first_result {
        let _ = pods.delete(first_name, &DeleteParams::default()).await;
        let _ = pvcs.delete(claim_name, &DeleteParams::default()).await;
        return Err(error);
    }
    let pv_name = pvcs
        .get(claim_name)
        .await?
        .spec
        .and_then(|spec| spec.volume_name);
    pods.delete(first_name, &DeleteParams::default()).await?;
    context
        .wait_until("first fsGroup policy Pod deletion", Duration::from_secs(240), || {
            let pods = pods.clone();
            async move { Ok(pods.get_opt(first_name).await?.is_none()) }
        })
        .await?;
    if let Some(pv_name) = pv_name {
        context
            .wait_until("first fsGroup policy VolumeAttachment deletion", Duration::from_secs(240), || {
                let attachments = attachments.clone();
                let pv_name = pv_name.clone();
                async move {
                    Ok(!attachments
                        .list(&ListParams::default())
                        .await?
                        .items
                        .into_iter()
                        .any(|attachment| {
                            attachment.spec.source.persistent_volume_name.as_deref()
                                == Some(pv_name.as_str())
                        }))
                }
            })
            .await?;
    }

    let second: Pod = create_pod(second_name)?;
    pods.create(&PostParams::default(), &second).await?;
    let result = context
        .wait_until("second fsGroup policy Pod", Duration::from_secs(240), || {
            let context = context.clone();
            async move {
                Ok(pod_termination_message(&context, second_name)
                    .await?
                    .is_some_and(|message| message.trim() == "4322"))
            }
        })
        .await;
    let _ = pods.delete(second_name, &DeleteParams::default()).await;
    let _ = pvcs.delete(claim_name, &DeleteParams::default()).await;
    result
}

pub(super) async fn pod_with_an_attach_required_pvc_waits_for_volumeattachment(
    context: &E2eContext,
) -> Result<()> {
    if crate::config::Config::from_env()?.nodelet_runtime() != "cri" {
        return Err(skip_test("CSI attach checks require the CRI runtime"));
    }
    let storage_class = std::env::var("TEST_CSI_ATTACH_STORAGE_CLASS")
        .ok()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            skip_test(
                "TEST_CSI_ATTACH_STORAGE_CLASS is not set; an attach-required CSI StorageClass is required",
            )
        })?;
    let pod_name = "csi-attach-check";
    let claim_name = "csi-attach-check-claim";
    let pods: Api<Pod> = Api::namespaced(context.client.clone(), &context.namespace);
    let pvcs: Api<PersistentVolumeClaim> =
        Api::namespaced(context.client.clone(), &context.namespace);
    let attachments: Api<VolumeAttachment> = Api::all(context.client.clone());
    let claim: PersistentVolumeClaim = serde_json::from_value(json!({
        "apiVersion": "v1",
        "kind": "PersistentVolumeClaim",
        "metadata": {"name": claim_name},
        "spec": {
            "accessModes": ["ReadWriteOnce"],
            "storageClassName": storage_class,
            "resources": {"requests": {"storage": "64Mi"}}
        }
    }))?;
    pvcs.create(&PostParams::default(), &claim).await?;
    let bind_result = context
        .wait_until("attach-required PVC to become Bound", Duration::from_secs(120), || {
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
            "containers": [{"name": "app", "image": "busybox:latest", "command": ["sleep", "3600"], "volumeMounts": [{"name": "data", "mountPath": "/data"}]}],
            "volumes": [{"name": "data", "persistentVolumeClaim": {"claimName": claim_name}}]
        }
    }))?;
    pods.create(&PostParams::default(), &pod).await?;
    let result = async {
        context
            .wait_until("attach-required PVC Pod Running", Duration::from_secs(120), || {
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
        let pv_name = pvcs
            .get(claim_name)
            .await?
            .spec
            .and_then(|spec| spec.volume_name)
            .context("attach-required PVC has no bound PV name")?;
        context
            .wait_until("VolumeAttachment for the bound PV", Duration::from_secs(60), || {
                let attachments = attachments.clone();
                let pv_name = pv_name.clone();
                async move {
                    Ok(attachments
                        .list(&ListParams::default())
                        .await?
                        .items
                        .into_iter()
                        .any(|attachment| {
                            attachment
                                .spec
                                .source
                                .persistent_volume_name
                                .as_deref()
                                == Some(pv_name.as_str())
                                && attachment
                                    .status
                                    .is_some_and(|status| status.attached)
                        }))
                }
            })
            .await
    }
    .await;
    let _ = pods.delete(pod_name, &DeleteParams::default()).await;
    let _ = pvcs.delete(claim_name, &DeleteParams::default()).await;
    result
}
