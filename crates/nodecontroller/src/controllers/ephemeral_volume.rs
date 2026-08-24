//! generic ephemeral-volume-controller: materializes each Pod's
//! `spec.volumes[].ephemeral.volumeClaimTemplate` as a PVC before nodelet
//! tries to mount it.
//!
//! Generic ephemeral volumes deliberately use the ordinary PVC/CSI path. The
//! controller-manager side creates a namespaced PVC named
//! `<pod-name>-<volume-name>`, copies the template's labels, annotations, and
//! spec, and makes the Pod its controller owner. The kubelet side then treats
//! that PVC like any other claim; garbage collection removes it with the Pod.
//!
//! This is separate from both CSI inline volumes (`spec.volumes[].csi`) and
//! OCI image volumes (`spec.volumes[].image`). Those have no PVC lifecycle for
//! this controller to manage.

use crate::workqueue::KeyedWorkQueue;
use anyhow::Result;
use futures::StreamExt;
use k8s_openapi::api::core::v1::{
    PersistentVolumeClaim, PersistentVolumeClaimTemplate, Pod, Volume,
};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{ObjectMeta, OwnerReference};
use kube::api::{Api, PostParams};
use kube::core::PartialObjectMeta;
use kube::runtime::watcher::Event;
use kube::{Client, ResourceExt};
use std::collections::HashMap;

/// The deterministic PVC name required by `EphemeralVolumeSource`.
pub fn ephemeral_pvc_name(pod_name: &str, volume_name: &str) -> String {
    format!("{pod_name}-{volume_name}")
}

fn owner_reference(pod: &Pod) -> Option<OwnerReference> {
    Some(OwnerReference {
        api_version: "v1".to_string(),
        kind: "Pod".to_string(),
        name: pod.name_any(),
        uid: pod.uid()?.to_string(),
        controller: Some(true),
        block_owner_deletion: Some(true),
    })
}

/// Build the PVC that backs one generic ephemeral volume. Only the template's
/// labels and annotations are copied: name, namespace, and ownership belong to
/// this controller and all other ObjectMeta fields are server-managed.
pub fn build_pvc(pod: &Pod, volume: &Volume) -> Option<PersistentVolumeClaim> {
    let template: &PersistentVolumeClaimTemplate =
        volume.ephemeral.as_ref()?.volume_claim_template.as_ref()?;
    let owner = owner_reference(pod)?;
    Some(PersistentVolumeClaim {
        metadata: ObjectMeta {
            name: Some(ephemeral_pvc_name(&pod.name_any(), &volume.name)),
            namespace: pod.namespace(),
            labels: template.metadata.as_ref().and_then(|m| m.labels.clone()),
            annotations: template
                .metadata
                .as_ref()
                .and_then(|m| m.annotations.clone()),
            owner_references: Some(vec![owner]),
            ..Default::default()
        },
        spec: Some(template.spec.clone()),
        ..Default::default()
    })
}

/// Whether `pvc` is controlled by exactly this Pod. A same-named PVC owned by
/// another object must never be adopted: using it would expose unrelated data
/// to the Pod and violate generic ephemeral-volume isolation.
pub fn pvc_owned_by_pod(pvc: &PersistentVolumeClaim, pod: &Pod) -> bool {
    pvc_metadata_owned_by_pod(&pvc.metadata, pod)
}

fn pvc_metadata_owned_by_pod(metadata: &ObjectMeta, pod: &Pod) -> bool {
    let Some(uid) = pod.uid() else { return false };
    metadata
        .owner_references
        .as_ref()
        .into_iter()
        .flatten()
        .any(|owner| {
            owner.controller == Some(true)
                && owner.kind == "Pod"
                && owner.name == pod.name_any()
                && owner.uid == uid
        })
}

fn metadata_changed(
    old: &PartialObjectMeta<PersistentVolumeClaim>,
    new: &PartialObjectMeta<PersistentVolumeClaim>,
) -> bool {
    let mut old_metadata = old.metadata.clone();
    let mut new_metadata = new.metadata.clone();
    // ResourceVersion and managedFields change for status-only writes. They
    // are not inputs to this controller, so don't rebuild its cache object or
    // requeue the Pod for those events.
    old_metadata.resource_version = None;
    old_metadata.managed_fields = None;
    new_metadata.resource_version = None;
    new_metadata.managed_fields = None;
    old_metadata != new_metadata
}

fn pod_reconcile_state_changed(old: &Pod, new: &Pod) -> bool {
    old.spec != new.spec
        || old.metadata.uid != new.metadata.uid
        || old.metadata.deletion_timestamp != new.metadata.deletion_timestamp
}

fn ephemeral_pvc_names(pod: &Pod) -> Vec<String> {
    pod.spec
        .as_ref()
        .and_then(|spec| spec.volumes.as_ref())
        .into_iter()
        .flatten()
        .filter(|volume| {
            volume
                .ephemeral
                .as_ref()
                .and_then(|source| source.volume_claim_template.as_ref())
                .is_some()
        })
        .map(|volume| ephemeral_pvc_name(&pod.name_any(), &volume.name))
        .collect()
}

fn key(namespace: &str, name: &str) -> String {
    format!("{namespace}/{name}")
}

fn pod_key(pod: &Pod) -> String {
    key(&pod.namespace().unwrap_or_default(), &pod.name_any())
}

async fn ensure_pvc(
    client: &kube::Client,
    pod: &Pod,
    volume: &Volume,
    pvc_cache: &HashMap<String, PartialObjectMeta<PersistentVolumeClaim>>,
) {
    let Some(pvc) = build_pvc(pod, volume) else {
        if volume.ephemeral.is_some() {
            tracing::warn!(
                namespace = ?pod.namespace(),
                pod = %pod.name_any(),
                volume = %volume.name,
                "generic ephemeral volume has no volumeClaimTemplate or the Pod has no UID"
            );
        }
        return;
    };
    let name = pvc.name_any();
    let namespace = pod.namespace().unwrap_or_default();
    let pvc_api: Api<PersistentVolumeClaim> = Api::namespaced(client.clone(), &namespace);
    let cache_key = key(&namespace, &name);

    if let Some(existing) = pvc_cache.get(&cache_key) {
        if !pvc_metadata_owned_by_pod(&existing.metadata, pod) {
            tracing::warn!(
                namespace = %namespace,
                pod = %pod.name_any(),
                pvc = %name,
                "generic ephemeral PVC already exists but is not owned by this Pod; refusing to adopt it"
            );
        }
        return;
    }

    match pvc_api.create(&PostParams::default(), &pvc).await {
        Ok(_) => {
            tracing::info!(namespace = %namespace, pod = %pod.name_any(), pvc = %name, "created generic ephemeral PVC");
        }
        Err(kube::Error::Api(status)) if status.is_already_exists() => {
            // A PVC event may not have reached this controller's cache yet.
            // Read the winner and apply the same ownership check before
            // allowing nodelet to use it.
            match pvc_api.get_metadata_opt(&name).await {
                Ok(Some(existing)) if pvc_metadata_owned_by_pod(&existing.metadata, pod) => {}
                Ok(Some(_)) => tracing::warn!(
                    namespace = %namespace,
                    pod = %pod.name_any(),
                    pvc = %name,
                    "generic ephemeral PVC create raced with an unrelated PVC; refusing to adopt it"
                ),
                Ok(None) => {
                    tracing::debug!(namespace = %namespace, pvc = %name, "generic ephemeral PVC disappeared after create race")
                }
                Err(error) => {
                    tracing::warn!(namespace = %namespace, pvc = %name, error = ?error, "failed to verify generic ephemeral PVC ownership after create race")
                }
            }
        }
        Err(error) => tracing::warn!(
            namespace = %namespace,
            pod = %pod.name_any(),
            pvc = %name,
            error = ?error,
            "failed to create generic ephemeral PVC"
        ),
    }
}

async fn reconcile_pod(
    client: &kube::Client,
    pod: &Pod,
    pvc_cache: &HashMap<String, PartialObjectMeta<PersistentVolumeClaim>>,
) {
    if pod.metadata.deletion_timestamp.is_some() {
        return;
    }
    let Some(volumes) = pod.spec.as_ref().and_then(|spec| spec.volumes.as_ref()) else {
        return;
    };
    for volume in volumes {
        if volume.ephemeral.is_some() {
            ensure_pvc(client, pod, volume, pvc_cache).await;
        }
    }
}

pub async fn run(client: Client, _cfg: &crate::config::Config) -> Result<()> {
    let mut pods: HashMap<String, Pod> = HashMap::new();
    let mut pvcs: HashMap<String, PartialObjectMeta<PersistentVolumeClaim>> = HashMap::new();
    let queue = KeyedWorkQueue::default();
    let mut pod_stream = crate::watch::watch_pods(&client);
    let mut pvc_stream = crate::watch::watch_persistent_volume_claim_metadata(&client);

    loop {
        tokio::select! {
            ev = pod_stream.next() => match ev {
                Some(Ok(Event::Apply(pod))) | Some(Ok(Event::InitApply(pod))) => {
                    let key = pod_key(&pod);
                    if pods.get(&key).is_none_or(|old| pod_reconcile_state_changed(old, &pod)) {
                        pods.insert(key.clone(), pod);
                        queue.enqueue(key);
                    }
                }
                Some(Ok(Event::Delete(pod))) => { pods.remove(&pod_key(&pod)); }
                Some(Ok(Event::Init | Event::InitDone)) => {}
                Some(Err(error)) => tracing::warn!(error = ?error, "Pod watch error in generic ephemeral-volume-controller"),
                None => return Ok(()),
            },
            ev = pvc_stream.next() => match ev {
                Some(Ok(Event::Apply(pvc))) | Some(Ok(Event::InitApply(pvc))) => {
                    let pvc_key = key(&pvc.namespace().unwrap_or_default(), &pvc.name_any());
                    let changed = pvcs.get(&pvc_key).is_none_or(|old| metadata_changed(old, &pvc));
                    if !changed {
                        continue;
                    }
                    pvcs.insert(pvc_key, pvc.clone());
                    let name = pvc.name_any();
                    let namespace = pvc.namespace().unwrap_or_default();
                    for pod in pods.values() {
                        if pod.namespace().unwrap_or_default() == namespace
                            && ephemeral_pvc_names(pod).iter().any(|expected| expected == &name)
                        {
                            queue.enqueue(pod_key(pod));
                        }
                    }
                }
                Some(Ok(Event::Delete(pvc))) => {
                    pvcs.remove(&key(&pvc.namespace().unwrap_or_default(), &pvc.name_any()));
                    let name = pvc.name_any();
                    let namespace = pvc.namespace().unwrap_or_default();
                    for pod in pods.values() {
                        if pod.namespace().unwrap_or_default() == namespace
                            && ephemeral_pvc_names(pod).iter().any(|expected| expected == &name)
                        {
                            queue.enqueue(pod_key(pod));
                        }
                    }
                }
                Some(Ok(Event::Init | Event::InitDone)) => {}
                Some(Err(error)) => tracing::warn!(error = ?error, "PVC watch error in generic ephemeral-volume-controller"),
                None => return Ok(()),
            },
            pod_key = queue.pop() => {
                if let Some(pod) = pods.get(&pod_key).cloned() {
                    reconcile_pod(&client, &pod, &pvcs).await;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use k8s_openapi::api::core::v1::{EphemeralVolumeSource, PodSpec};
    use std::collections::BTreeMap;

    fn pod() -> Pod {
        Pod {
            metadata: ObjectMeta {
                name: Some("app".to_string()),
                namespace: Some("test".to_string()),
                uid: Some("pod-uid".to_string()),
                ..Default::default()
            },
            spec: Some(PodSpec {
                volumes: Some(vec![Volume {
                    name: "config".to_string(),
                    ephemeral: Some(EphemeralVolumeSource {
                        volume_claim_template: Some(PersistentVolumeClaimTemplate {
                            metadata: Some(ObjectMeta {
                                labels: Some(BTreeMap::from([(
                                    "role".to_string(),
                                    "config".to_string(),
                                )])),
                                annotations: Some(BTreeMap::from([(
                                    "owner".to_string(),
                                    "test".to_string(),
                                )])),
                                ..Default::default()
                            }),
                            ..Default::default()
                        }),
                    }),
                    ..Default::default()
                }]),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    #[test]
    fn pvc_name_is_pod_and_volume_scoped() {
        assert_eq!(ephemeral_pvc_name("app", "config"), "app-config");
    }

    #[test]
    fn build_pvc_copies_claim_data_and_controls_it_with_the_pod() {
        let pod = pod();
        let volume = pod
            .spec
            .as_ref()
            .unwrap()
            .volumes
            .as_ref()
            .unwrap()
            .first()
            .unwrap();
        let pvc = build_pvc(&pod, volume).expect("Pod has a UID and a claim template");

        assert_eq!(pvc.name_any(), "app-config");
        assert_eq!(pvc.namespace().as_deref(), Some("test"));
        assert_eq!(pvc.metadata.labels.as_ref().unwrap()["role"], "config");
        assert_eq!(pvc.metadata.annotations.as_ref().unwrap()["owner"], "test");
        assert!(pvc_owned_by_pod(&pvc, &pod));
        assert_eq!(pvc.metadata.owner_references.as_ref().unwrap().len(), 1);
    }

    #[test]
    fn an_existing_pvc_owned_by_another_pod_is_not_adoptable() {
        let pod = pod();
        let volume = pod
            .spec
            .as_ref()
            .unwrap()
            .volumes
            .as_ref()
            .unwrap()
            .first()
            .unwrap();
        let mut pvc = build_pvc(&pod, volume).unwrap();
        pvc.metadata.owner_references.as_mut().unwrap()[0].uid = "different-uid".to_string();
        assert!(!pvc_owned_by_pod(&pvc, &pod));
    }
}
