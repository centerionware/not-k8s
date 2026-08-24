use super::context::E2eContext;
use anyhow::Result;
use k8s_openapi::api::core::v1::{PersistentVolume, PersistentVolumeClaim};
use k8s_openapi::api::storage::v1::StorageClass;
use kube::api::{Api, DeleteParams, PostParams};
use serde_json::json;
use std::time::Duration;

pub(super) async fn pv_binder_binds_a_static_pv_and_protection_finalizers_gate_deletion(
    context: &E2eContext,
) -> Result<()> {
    let class = "static-e2e-class";
    let pv_name = "static-e2e-pv";
    let pvc_name = "static-e2e-pvc";
    let pvs: Api<PersistentVolume> = Api::all(context.client.clone());
    let pv: PersistentVolume = serde_json::from_value(json!({
        "apiVersion": "v1",
        "kind": "PersistentVolume",
        "metadata": {"name": pv_name},
        "spec": {"capacity": {"storage": "10Mi"}, "accessModes": ["ReadWriteOnce"], "storageClassName": class, "persistentVolumeReclaimPolicy": "Delete", "hostPath": {"path": "/tmp/not-k8s-e2e-static-pv"}}
    }))?;
    pvs.create(&PostParams::default(), &pv).await?;
    let pvcs: Api<PersistentVolumeClaim> =
        Api::namespaced(context.client.clone(), &context.namespace);
    let pvc: PersistentVolumeClaim = serde_json::from_value(json!({
        "apiVersion": "v1",
        "kind": "PersistentVolumeClaim",
        "metadata": {"name": pvc_name},
        "spec": {"accessModes": ["ReadWriteOnce"], "storageClassName": class, "resources": {"requests": {"storage": "10Mi"}}}
    }))?;
    pvcs.create(&PostParams::default(), &pvc).await?;
    let wait_result = context
        .wait_until("static PVC to bind", Duration::from_secs(90), || {
            let pvcs = pvcs.clone();
            let pvs = pvs.clone();
            async move {
                let claim = pvcs.get(pvc_name).await?;
                let volume = pvs.get(pv_name).await?;
                let bound_volume = claim
                    .spec
                    .as_ref()
                    .and_then(|spec| spec.volume_name.as_deref())
                    == Some(pv_name);
                let bound_phase = claim
                        .status
                        .as_ref()
                        .and_then(|status| status.phase)
                        .as_deref()
                        == Some("Bound");
                Ok(bound_volume
                    && bound_phase
                    && volume
                        .metadata
                        .finalizers
                        .unwrap_or_default()
                        .iter()
                        .any(|finalizer| finalizer == "kubernetes.io/pv-protection"))
            }
        })
        .await;
    let _ = pvcs.delete(pvc_name, &DeleteParams::default()).await;
    let _ = pvs.delete(pv_name, &DeleteParams::default()).await;
    wait_result
}

pub(super) async fn pv_binder_requests_dynamic_provisioning_from_storage_class(
    context: &E2eContext,
) -> Result<()> {
    let class = "dynamic-e2e-class";
    let pvc_name = "dynamic-e2e-pvc";
    let provisioner = "not-k8s.test/fake-provisioner";
    let classes: Api<StorageClass> = Api::all(context.client.clone());
    let storage_class: StorageClass = serde_json::from_value(json!({
        "apiVersion": "storage.k8s.io/v1",
        "kind": "StorageClass",
        "metadata": {"name": class},
        "provisioner": provisioner,
        "volumeBindingMode": "Immediate"
    }))?;
    classes
        .create(&PostParams::default(), &storage_class)
        .await?;
    let pvcs: Api<PersistentVolumeClaim> =
        Api::namespaced(context.client.clone(), &context.namespace);
    let pvc: PersistentVolumeClaim = serde_json::from_value(json!({
        "apiVersion": "v1",
        "kind": "PersistentVolumeClaim",
        "metadata": {"name": pvc_name},
        "spec": {"accessModes": ["ReadWriteOnce"], "storageClassName": class, "resources": {"requests": {"storage": "10Mi"}}}
    }))?;
    pvcs.create(&PostParams::default(), &pvc).await?;
    let wait_result = context
        .wait_until("PVC to receive the StorageClass provisioner", Duration::from_secs(90), || {
            let pvcs = pvcs.clone();
            async move {
                Ok(pvcs
                    .get(pvc_name)
                    .await?
                    .metadata
                    .annotations
                    .unwrap_or_default()
                    .get("volume.kubernetes.io/storage-provisioner")
                    .is_some_and(|value| value == provisioner))
            }
        })
        .await;
    let _ = pvcs.delete(pvc_name, &DeleteParams::default()).await;
    let _ = classes.delete(class, &DeleteParams::default()).await;
    wait_result
}
