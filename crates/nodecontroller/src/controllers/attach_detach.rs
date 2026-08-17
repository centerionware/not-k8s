//! attach-detach-controller (Group G, Tier 0): creates and deletes
//! `storage.k8s.io/v1 VolumeAttachment` objects so that a CSI driver's
//! external-attacher sidecar (a real, separate component — this crate
//! never speaks CSI's `ControllerPublishVolume`/`ControllerUnpublishVolume`
//! RPCs itself) actually attaches a CSI volume to the node a Pod using it
//! is scheduled on. Without this, nodelet's own consumer side (the
//! `volumes.kubernetes.io/controller-managed-attach-detach` Node
//! annotation nodelet already sets — see `crates/nodelet/src/node.rs`)
//! has nothing to wait on: no `VolumeAttachment` is ever created, so the
//! external-attacher never acts, and every CSI-backed volume test that
//! depends on a real attach silently breaks the moment k3s's own
//! controller-manager (which does this today) is disabled.
//!
//! # The model: desired vs. actual, recomputed on every relevant event
//!
//! Unlike this crate's other controllers, there's no single natural "key"
//! object to reconcile — a `VolumeAttachment` is needed because of a
//! Pod↔PVC↔PV relationship, not because any one of those three changed in
//! isolation. So this controller keeps upstream's own shape: a desired set
//! of `(driver, PV name, node)` triples computed fresh from the current
//! Pod/PVC/PV caches on every Pod, PVC, PV, or VolumeAttachment watch
//! event, diffed against the actual `VolumeAttachment` set, create the
//! shortfall and delete the excess. Cluster-wide cardinality here is
//! "attached volumes," typically small, so a full recompute per event is
//! simpler than incremental bookkeeping and cheap enough not to matter.
//!
//! `VolumeAttachment` names are this crate's own deterministic FNV hash of
//! the triple (`va-<hash>`), not upstream's exact naming scheme — nothing
//! reads a `VolumeAttachment`'s name for meaning, only its `spec`.
//!
//! # Scope of this slice
//!
//! **CSI volumes only** — a PV's `spec.csi` is the only volume source this
//! controller understands. In-tree volume plugins (`awsElasticBlockStore`,
//! `gcePersistentDisk`, ...) are all deprecated/removed upstream in favor
//! of CSI migration, and this project has no cloud provider integration to
//! begin with (see `CLAUDE.md`), so there is nothing to migrate away from.
//!
//! **A Pod "wants" its volumes attached from the moment it's scheduled
//! (`spec.nodeName` set) until it is fully removed from the apiserver** —
//! not just until `deletionTimestamp` is set. This is deliberately
//! conservative: releasing a volume mid-graceful-termination (while a
//! container might still be writing to it) would be a real correctness
//! bug, and nodelet's own teardown already runs the terminationGracePeriod
//! before the Pod object disappears (see commit `9264e11`), so "the Pod is
//! gone" is already the right signal to wait for.
//!
//! **No `VolumeAttachment.status` is read or waited on by this
//! controller** — creating the object and handing it to the
//! external-attacher is this controller's entire job; nodelet is the one
//! that actually waits on `status.attached` before mounting (that's the
//! kubelet-side volume manager's job, a separate concern from this
//! controller-manager-side one).
//!
//! **No force-detach-on-unresponsive-node timer** — upstream has a
//! `--node-detach-timeout`-driven fallback that force-detaches when a node
//! stops reporting entirely (heartbeat lease expired) rather than waiting
//! forever for the Pod to be cleanly removed from it. Not implemented here:
//! a genuinely unreachable node's volumes simply stay attached until
//! `node-lifecycle-controller`'s own pod eviction removes the Pods, which
//! then naturally clears this controller's desired set. A real, named gap
//! for the "node vanished and never comes back" case specifically.
//!
//! **Depends on `persistentvolume-binder-controller` actually binding a PV
//! first** (no PV, no attachment to create) — see that file's own module
//! doc for an open, live-CI-diagnosed verification gap in the
//! provisioner-prebound (dynamic CSI) path this controller's own e2e
//! coverage (`csi_attach.sh`) currently inherits.

use anyhow::Result;
use futures::StreamExt;
use k8s_openapi::api::core::v1::{PersistentVolume, PersistentVolumeClaim, Pod};
use k8s_openapi::api::storage::v1::{
    VolumeAttachment, VolumeAttachmentSource, VolumeAttachmentSpec,
};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use kube::api::{Api, DeleteParams, PostParams};
use kube::runtime::watcher::Event;
use kube::{Client, ResourceExt};
use std::collections::{HashMap, HashSet};

/// `(driver, PV name, node name)` — everything a `VolumeAttachment`'s spec
/// needs, and exactly what determines whether two desired attachments are
/// "the same" one.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct AttachmentKey {
    pub driver: String,
    pub pv_name: String,
    pub node_name: String,
}

fn va_name(key: &AttachmentKey) -> String {
    let bytes = format!("{}\0{}\0{}", key.driver, key.pv_name, key.node_name).into_bytes();
    let mut hash: u64 = 0xcbf29ce484222325;
    for b in bytes {
        hash ^= b as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("va-{hash:x}")
}

/// Which CSI `(driver, PV name)` a bound PVC resolves to, if any — `None`
/// for an unbound PVC or a PV with no `spec.csi` (in-tree/other volume
/// types, out of scope — see module doc).
fn csi_pv_for_claim(
    pvc: &PersistentVolumeClaim,
    pvs: &HashMap<String, PersistentVolume>,
) -> Option<(String, String)> {
    let pv_name = pvc.spec.as_ref()?.volume_name.as_ref()?;
    let pv = pvs.get(pv_name)?;
    let csi = pv.spec.as_ref()?.csi.as_ref()?;
    Some((csi.driver.clone(), pv_name.clone()))
}

/// The full desired set: one entry per (CSI-backed PV, node) pair a live,
/// scheduled Pod currently references. Pure function of the three caches —
/// the reconcile loop's whole job is diffing this against reality.
pub fn desired_attachments(
    pods: &HashMap<String, Pod>,
    pvcs: &HashMap<String, PersistentVolumeClaim>,
    pvs: &HashMap<String, PersistentVolume>,
) -> HashSet<AttachmentKey> {
    let mut desired = HashSet::new();
    for pod in pods.values() {
        let Some(node_name) = pod.spec.as_ref().and_then(|s| s.node_name.clone()) else {
            continue;
        };
        let namespace = pod.namespace().unwrap_or_default();
        let Some(volumes) = pod.spec.as_ref().and_then(|s| s.volumes.as_ref()) else {
            continue;
        };
        for vol in volumes {
            let Some(claim) = vol.persistent_volume_claim.as_ref() else {
                continue;
            };
            let Some(pvc) = pvcs.get(&format!("{namespace}/{}", claim.claim_name)) else {
                continue;
            };
            let Some((driver, pv_name)) = csi_pv_for_claim(pvc, pvs) else {
                continue;
            };
            desired.insert(AttachmentKey {
                driver,
                pv_name,
                node_name: node_name.clone(),
            });
        }
    }
    desired
}

fn build_volume_attachment(key: &AttachmentKey) -> VolumeAttachment {
    VolumeAttachment {
        metadata: ObjectMeta {
            name: Some(va_name(key)),
            ..Default::default()
        },
        spec: VolumeAttachmentSpec {
            attacher: key.driver.clone(),
            node_name: key.node_name.clone(),
            source: VolumeAttachmentSource {
                persistent_volume_name: Some(key.pv_name.clone()),
                ..Default::default()
            },
        },
        status: None,
    }
}

async fn reconcile(
    va_api: &Api<VolumeAttachment>,
    pods: &HashMap<String, Pod>,
    pvcs: &HashMap<String, PersistentVolumeClaim>,
    pvs: &HashMap<String, PersistentVolume>,
    existing: &HashMap<String, VolumeAttachment>,
) {
    let desired = desired_attachments(pods, pvcs, pvs);
    let desired_names: HashSet<String> = desired.iter().map(va_name).collect();

    for key in &desired {
        let name = va_name(key);
        if existing.contains_key(&name) {
            continue;
        }
        let va = build_volume_attachment(key);
        match va_api.create(&PostParams::default(), &va).await {
            Ok(_) => {
                tracing::info!(name = %name, driver = %key.driver, pv = %key.pv_name, node = %key.node_name, "attach-detach-controller created a VolumeAttachment")
            }
            Err(kube::Error::Api(ref e)) if e.is_already_exists() => {}
            Err(e) => {
                tracing::warn!(name = %name, error = ?e, "attach-detach-controller failed to create a VolumeAttachment")
            }
        }
    }

    for name in existing.keys() {
        // The apiserver may contain VolumeAttachments created by another
        // controller or an operator. This controller owns only its stable
        // `va-<hash>` names, so never detach anything outside that namespace.
        if desired_names.contains(name) || !name.starts_with("va-") {
            continue;
        }
        match va_api.delete(name, &DeleteParams::default()).await {
            Ok(_) => {
                tracing::info!(name = %name, "attach-detach-controller deleted an unneeded VolumeAttachment")
            }
            Err(kube::Error::Api(ref e)) if e.is_not_found() => {}
            Err(e) => {
                tracing::warn!(name = %name, error = ?e, "attach-detach-controller failed to delete a VolumeAttachment")
            }
        }
    }
}

fn ns_key<K: ResourceExt>(obj: &K) -> String {
    format!("{}/{}", obj.namespace().unwrap_or_default(), obj.name_any())
}

pub async fn run(client: Client, _cfg: &crate::config::Config) -> Result<()> {
    let mut pods: HashMap<String, Pod> = HashMap::new();
    let mut pvcs: HashMap<String, PersistentVolumeClaim> = HashMap::new();
    let mut pvs: HashMap<String, PersistentVolume> = HashMap::new();
    let mut vas: HashMap<String, VolumeAttachment> = HashMap::new();

    let va_api: Api<VolumeAttachment> = Api::all(client.clone());

    let mut pod_stream = crate::watch::watch_pods(&client);
    let mut pvc_stream = crate::watch::watch_persistent_volume_claims(&client);
    let mut pv_stream = crate::watch::watch_persistent_volumes(&client);
    let mut va_stream = crate::watch::watch_volume_attachments(&client);

    loop {
        let dirty = tokio::select! {
            ev = pod_stream.next() => match ev {
                Some(Ok(Event::Apply(p))) | Some(Ok(Event::InitApply(p))) => { pods.insert(ns_key(&p), p); true }
                Some(Ok(Event::Delete(p))) => { pods.remove(&ns_key(&p)); true }
                Some(Ok(Event::Init | Event::InitDone)) => false,
                Some(Err(e)) => { tracing::warn!(error = ?e, "pod watch error in attach-detach-controller"); false }
                None => return Ok(()),
            },
            ev = pvc_stream.next() => match ev {
                Some(Ok(Event::Apply(c))) | Some(Ok(Event::InitApply(c))) => { pvcs.insert(ns_key(&c), c); true }
                Some(Ok(Event::Delete(c))) => { pvcs.remove(&ns_key(&c)); true }
                Some(Ok(Event::Init | Event::InitDone)) => false,
                Some(Err(e)) => { tracing::warn!(error = ?e, "pvc watch error in attach-detach-controller"); false }
                None => return Ok(()),
            },
            ev = pv_stream.next() => match ev {
                Some(Ok(Event::Apply(v))) | Some(Ok(Event::InitApply(v))) => { pvs.insert(v.name_any(), v); true }
                Some(Ok(Event::Delete(v))) => { pvs.remove(&v.name_any()); true }
                Some(Ok(Event::Init | Event::InitDone)) => false,
                Some(Err(e)) => { tracing::warn!(error = ?e, "pv watch error in attach-detach-controller"); false }
                None => return Ok(()),
            },
            ev = va_stream.next() => match ev {
                Some(Ok(Event::Apply(v))) | Some(Ok(Event::InitApply(v))) => { vas.insert(v.name_any(), v); true }
                Some(Ok(Event::Delete(v))) => { vas.remove(&v.name_any()); true }
                Some(Ok(Event::Init | Event::InitDone)) => false,
                Some(Err(e)) => { tracing::warn!(error = ?e, "volumeattachment watch error in attach-detach-controller"); false }
                None => return Ok(()),
            },
        };
        if dirty {
            reconcile(&va_api, &pods, &pvcs, &pvs, &vas).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use k8s_openapi::api::core::v1::{
        CSIPersistentVolumeSource, PersistentVolumeClaimSpec, PersistentVolumeClaimVolumeSource,
        PersistentVolumeSpec, PodSpec, Volume,
    };

    fn pod(name: &str, node: Option<&str>, claim: &str) -> Pod {
        Pod {
            metadata: ObjectMeta {
                name: Some(name.to_string()),
                namespace: Some("default".to_string()),
                ..Default::default()
            },
            spec: Some(PodSpec {
                node_name: node.map(str::to_string),
                volumes: Some(vec![Volume {
                    name: "data".to_string(),
                    persistent_volume_claim: Some(PersistentVolumeClaimVolumeSource {
                        claim_name: claim.to_string(),
                        read_only: None,
                    }),
                    ..Default::default()
                }]),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    fn bound_csi_pvc(name: &str, pv_name: &str) -> PersistentVolumeClaim {
        PersistentVolumeClaim {
            metadata: ObjectMeta {
                name: Some(name.to_string()),
                namespace: Some("default".to_string()),
                ..Default::default()
            },
            spec: Some(PersistentVolumeClaimSpec {
                volume_name: Some(pv_name.to_string()),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    fn csi_pv(name: &str, driver: &str) -> PersistentVolume {
        PersistentVolume {
            metadata: ObjectMeta {
                name: Some(name.to_string()),
                ..Default::default()
            },
            spec: Some(PersistentVolumeSpec {
                csi: Some(CSIPersistentVolumeSource {
                    driver: driver.to_string(),
                    volume_handle: "vol-1".to_string(),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    #[test]
    fn a_scheduled_pod_with_a_bound_csi_pvc_wants_an_attachment() {
        let pods = HashMap::from([("default/p1".to_string(), pod("p1", Some("node-a"), "c1"))]);
        let pvcs = HashMap::from([("default/c1".to_string(), bound_csi_pvc("c1", "pv-1"))]);
        let pvs = HashMap::from([("pv-1".to_string(), csi_pv("pv-1", "disk.csi.example.com"))]);
        let desired = desired_attachments(&pods, &pvcs, &pvs);
        assert_eq!(
            desired,
            HashSet::from([AttachmentKey {
                driver: "disk.csi.example.com".to_string(),
                pv_name: "pv-1".to_string(),
                node_name: "node-a".to_string()
            }])
        );
    }

    #[test]
    fn an_unscheduled_pod_wants_nothing() {
        let pods = HashMap::from([("default/p1".to_string(), pod("p1", None, "c1"))]);
        let pvcs = HashMap::from([("default/c1".to_string(), bound_csi_pvc("c1", "pv-1"))]);
        let pvs = HashMap::from([("pv-1".to_string(), csi_pv("pv-1", "disk.csi.example.com"))]);
        assert!(desired_attachments(&pods, &pvcs, &pvs).is_empty());
    }

    #[test]
    fn an_unbound_pvc_wants_nothing() {
        let pods = HashMap::from([("default/p1".to_string(), pod("p1", Some("node-a"), "c1"))]);
        let unbound = PersistentVolumeClaim {
            metadata: ObjectMeta {
                name: Some("c1".to_string()),
                namespace: Some("default".to_string()),
                ..Default::default()
            },
            spec: Some(PersistentVolumeClaimSpec::default()),
            ..Default::default()
        };
        let pvcs = HashMap::from([("default/c1".to_string(), unbound)]);
        assert!(desired_attachments(&pods, &pvcs, &HashMap::new()).is_empty());
    }

    #[test]
    fn a_non_csi_pv_wants_nothing() {
        let pods = HashMap::from([("default/p1".to_string(), pod("p1", Some("node-a"), "c1"))]);
        let pvcs = HashMap::from([("default/c1".to_string(), bound_csi_pvc("c1", "pv-1"))]);
        let non_csi = PersistentVolume {
            metadata: ObjectMeta {
                name: Some("pv-1".to_string()),
                ..Default::default()
            },
            spec: Some(PersistentVolumeSpec::default()),
            ..Default::default()
        };
        let pvs = HashMap::from([("pv-1".to_string(), non_csi)]);
        assert!(desired_attachments(&pods, &pvcs, &pvs).is_empty());
    }

    #[test]
    fn two_pods_on_the_same_node_sharing_a_pvc_dedupe_to_one_attachment() {
        let pods = HashMap::from([
            ("default/p1".to_string(), pod("p1", Some("node-a"), "c1")),
            ("default/p2".to_string(), pod("p2", Some("node-a"), "c1")),
        ]);
        let pvcs = HashMap::from([("default/c1".to_string(), bound_csi_pvc("c1", "pv-1"))]);
        let pvs = HashMap::from([("pv-1".to_string(), csi_pv("pv-1", "disk.csi.example.com"))]);
        assert_eq!(desired_attachments(&pods, &pvcs, &pvs).len(), 1);
    }

    #[test]
    fn attachment_name_is_deterministic_and_key_sensitive() {
        let a = AttachmentKey {
            driver: "d".to_string(),
            pv_name: "pv".to_string(),
            node_name: "n".to_string(),
        };
        let b = AttachmentKey {
            driver: "d".to_string(),
            pv_name: "pv".to_string(),
            node_name: "n2".to_string(),
        };
        assert_eq!(va_name(&a), va_name(&a));
        assert_ne!(va_name(&a), va_name(&b));
    }
}
