//! pv-protection-controller / pvc-protection-controller (Group G, Tier 2):
//! the standard Kubernetes finalizer-based "don't let this disappear while
//! something still needs it" pattern, applied to `PersistentVolume` (in
//! use = still `Bound`) and `PersistentVolumeClaim` (in use = a live Pod
//! still references it by name). Both run from this one file since they're
//! the same shape at half the size each — not because they interact.
//!
//! # Scope of this slice
//!
//! **A PVC is "in use" by name only, not by resolving through
//! ephemeral/generic volume sources** — a Pod's `spec.volumes[].persistentVolumeClaim`
//! is the only reference this controller looks for, matching what
//! `attach-detach-controller`'s own desired-state computation already
//! looks for (same project-wide scope, not a new gap introduced here).
//!
//! **No admission-time rejection** — upstream additionally has an
//! admission plugin that rejects a delete of a finalizer-protected object
//! outright; this controller only manages the finalizer itself. A
//! `kubectl delete` on an in-use PV/PVC still appears to "work" (sets
//! `deletionTimestamp`) but the object stays present until the finalizer
//! is removed — the intended protection still holds, just via one
//! mechanism instead of two.

use anyhow::Result;
use crate::workqueue::KeyedWorkQueue;
use futures::StreamExt;
use kube::api::{Api, Patch, PatchParams};
use kube::runtime::watcher::Event;
use kube::{Client, ResourceExt};
use k8s_openapi::api::core::v1::{PersistentVolume, PersistentVolumeClaim, Pod};
use std::collections::HashMap;

const PV_PROTECTION_FINALIZER: &str = "kubernetes.io/pv-protection";
const PVC_PROTECTION_FINALIZER: &str = "kubernetes.io/pvc-protection";

fn has_finalizer(finalizers: &Option<Vec<String>>, target: &str) -> bool {
    finalizers.as_ref().is_some_and(|f| f.iter().any(|x| x == target))
}

fn without_finalizer(finalizers: &Option<Vec<String>>, target: &str) -> Vec<String> {
    finalizers.as_ref().into_iter().flatten().filter(|f| f.as_str() != target).cloned().collect()
}

async fn reconcile_pv(client: &Client, pv: &PersistentVolume, pvcs: &HashMap<String, PersistentVolumeClaim>) {
    let name = pv.name_any();
    let api: Api<PersistentVolume> = Api::all(client.clone());

    if pv.metadata.deletion_timestamp.is_none() {
        if !has_finalizer(&pv.metadata.finalizers, PV_PROTECTION_FINALIZER) {
            let mut finalizers = pv.metadata.finalizers.clone().unwrap_or_default();
            finalizers.push(PV_PROTECTION_FINALIZER.to_string());
            let patch = serde_json::json!({ "metadata": { "finalizers": finalizers } });
            if let Err(e) = api.patch(&name, &PatchParams::default(), &Patch::Merge(&patch)).await {
                tracing::warn!(pv = %name, error = ?e, "failed to add pv-protection finalizer");
            }
        }
        return;
    }

    // "In use" means claimRef still points at a PVC that actually exists —
    // not `status.phase == "Bound"`. This controller never transitions a
    // PV's phase back to `Released` once its PVC is deleted (see the
    // module doc's "no unbind/reclaim" scope note), so keying protection
    // off phase would leave every bound PV permanently undeletable, even
    // after its PVC is long gone. Caught in review before it ever shipped
    // as a real deadlock.
    let claim_ref = pv.spec.as_ref().and_then(|s| s.claim_ref.as_ref());
    let in_use = claim_ref.is_some_and(|r| {
        let ns = r.namespace.clone().unwrap_or_default();
        let name = r.name.clone().unwrap_or_default();
        pvcs.contains_key(&format!("{ns}/{name}"))
    });
    if in_use || !has_finalizer(&pv.metadata.finalizers, PV_PROTECTION_FINALIZER) {
        return;
    }
    let remaining = without_finalizer(&pv.metadata.finalizers, PV_PROTECTION_FINALIZER);
    let patch = serde_json::json!({ "metadata": { "finalizers": remaining } });
    if let Err(e) = api.patch(&name, &PatchParams::default(), &Patch::Merge(&patch)).await {
        tracing::warn!(pv = %name, error = ?e, "failed to remove pv-protection finalizer");
    }
}

fn pvc_in_use(namespace: &str, name: &str, pods: &HashMap<String, Pod>) -> bool {
    pods.values().filter(|p| p.namespace().as_deref() == Some(namespace)).filter(|p| p.metadata.deletion_timestamp.is_none()).any(|p| {
        p.spec.as_ref().and_then(|s| s.volumes.as_ref()).into_iter().flatten().any(|v| v.persistent_volume_claim.as_ref().is_some_and(|c| c.claim_name == name))
    })
}

async fn reconcile_pvc(client: &Client, pvc: &PersistentVolumeClaim, pods: &HashMap<String, Pod>) {
    let namespace = ns_of(pvc);
    let name = pvc.name_any();
    let api: Api<PersistentVolumeClaim> = Api::namespaced(client.clone(), &namespace);

    if pvc.metadata.deletion_timestamp.is_none() {
        if !has_finalizer(&pvc.metadata.finalizers, PVC_PROTECTION_FINALIZER) {
            let mut finalizers = pvc.metadata.finalizers.clone().unwrap_or_default();
            finalizers.push(PVC_PROTECTION_FINALIZER.to_string());
            let patch = serde_json::json!({ "metadata": { "finalizers": finalizers } });
            if let Err(e) = api.patch(&name, &PatchParams::default(), &Patch::Merge(&patch)).await {
                tracing::warn!(namespace = %namespace, pvc = %name, error = ?e, "failed to add pvc-protection finalizer");
            }
        }
        return;
    }

    if pvc_in_use(&namespace, &name, pods) || !has_finalizer(&pvc.metadata.finalizers, PVC_PROTECTION_FINALIZER) {
        return;
    }
    let remaining = without_finalizer(&pvc.metadata.finalizers, PVC_PROTECTION_FINALIZER);
    let patch = serde_json::json!({ "metadata": { "finalizers": remaining } });
    if let Err(e) = api.patch(&name, &PatchParams::default(), &Patch::Merge(&patch)).await {
        tracing::warn!(namespace = %namespace, pvc = %name, error = ?e, "failed to remove pvc-protection finalizer");
    }
}

fn ns_of<K: ResourceExt>(obj: &K) -> String {
    obj.namespace().unwrap_or_default()
}

pub async fn run(client: Client, _cfg: &crate::config::Config) -> Result<()> {
    let mut pods: HashMap<String, Pod> = HashMap::new();
    let mut pvcs: HashMap<String, PersistentVolumeClaim> = HashMap::new();
    let mut pvs: HashMap<String, PersistentVolume> = HashMap::new();
    let queue: KeyedWorkQueue<String> = KeyedWorkQueue::default();

    let mut pod_stream = crate::watch::watch_pods(&client);
    let mut pvc_stream = crate::watch::watch_persistent_volume_claims(&client);
    let mut pv_stream = crate::watch::watch_persistent_volumes(&client);

    loop {
        tokio::select! {
            ev = pod_stream.next() => {
                match ev {
                    Some(Ok(Event::Apply(pod))) | Some(Ok(Event::InitApply(pod))) => {
                        let ns = ns_of(&pod);
                        pods.insert(format!("{ns}/{}", pod.name_any()), pod);
                        for (key, _pvc) in pvcs.iter().filter(|(_, pvc)| ns_of(*pvc) == ns) {
                            queue.enqueue(format!("pvc:{key}"));
                        }
                    }
                    Some(Ok(Event::Delete(pod))) => {
                        let ns = ns_of(&pod);
                        pods.remove(&format!("{ns}/{}", pod.name_any()));
                        for (key, _pvc) in pvcs.iter().filter(|(_, pvc)| ns_of(*pvc) == ns) {
                            queue.enqueue(format!("pvc:{key}"));
                        }
                    }
                    Some(Ok(Event::Init | Event::InitDone)) => {}
                    Some(Err(e)) => tracing::warn!(error = ?e, "pod watch error in storage-protection"),
                    None => return Ok(()),
                }
            }
            ev = pvc_stream.next() => {
                match ev {
                    Some(Ok(Event::Apply(pvc))) | Some(Ok(Event::InitApply(pvc))) => {
                        let ns = ns_of(&pvc);
                        let name = pvc.name_any();
                        let key = format!("{ns}/{name}");
                        pvcs.insert(key.clone(), pvc);
                        queue.enqueue(format!("pvc:{key}"));
                    }
                    Some(Ok(Event::Delete(pvc))) => {
                        let key = format!("{}/{}", ns_of(&pvc), pvc.name_any());
                        pvcs.remove(&key);
                        // A PV being deleted might have been waiting on
                        // exactly this PVC — re-check every PV's
                        // finalizer now that it's gone, not just the ones
                        // that happen to fire their own watch event.
                        for name in pvs.keys() {
                            queue.enqueue(format!("pv:{name}"));
                        }
                    }
                    Some(Ok(Event::Init | Event::InitDone)) => {}
                    Some(Err(e)) => tracing::warn!(error = ?e, "pvc watch error in storage-protection"),
                    None => return Ok(()),
                }
            }
            ev = pv_stream.next() => {
                match ev {
                    Some(Ok(Event::Apply(pv))) | Some(Ok(Event::InitApply(pv))) => {
                        let name = pv.name_any();
                        pvs.insert(name.clone(), pv);
                        queue.enqueue(format!("pv:{name}"));
                    }
                    Some(Ok(Event::Delete(pv))) => { pvs.remove(&pv.name_any()); }
                    Some(Ok(Event::Init | Event::InitDone)) => {}
                    Some(Err(e)) => tracing::warn!(error = ?e, "pv watch error in storage-protection"),
                    None => return Ok(()),
                }
            }
            key = queue.pop() => {
                if let Some(name) = key.strip_prefix("pv:") {
                    if let Some(pv) = pvs.get(name).cloned() {
                        reconcile_pv(&client, &pv, &pvcs).await;
                    }
                } else if let Some(name) = key.strip_prefix("pvc:") {
                    if let Some(pvc) = pvcs.get(name).cloned() {
                        reconcile_pvc(&client, &pvc, &pods).await;
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use k8s_openapi::api::core::v1::{PersistentVolumeClaimVolumeSource, PodSpec, Volume};
    use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;

    #[test]
    fn finalizer_presence_check() {
        assert!(has_finalizer(&Some(vec!["a".to_string(), PV_PROTECTION_FINALIZER.to_string()]), PV_PROTECTION_FINALIZER));
        assert!(!has_finalizer(&Some(vec!["a".to_string()]), PV_PROTECTION_FINALIZER));
        assert!(!has_finalizer(&None, PV_PROTECTION_FINALIZER));
    }

    #[test]
    fn removing_a_finalizer_keeps_the_others() {
        let f = Some(vec!["a".to_string(), PV_PROTECTION_FINALIZER.to_string(), "b".to_string()]);
        assert_eq!(without_finalizer(&f, PV_PROTECTION_FINALIZER), vec!["a".to_string(), "b".to_string()]);
    }

    fn pod_with_pvc(ns: &str, claim: &str) -> Pod {
        Pod {
            metadata: ObjectMeta { name: Some("p".to_string()), namespace: Some(ns.to_string()), ..Default::default() },
            spec: Some(PodSpec {
                volumes: Some(vec![Volume { name: "data".to_string(), persistent_volume_claim: Some(PersistentVolumeClaimVolumeSource { claim_name: claim.to_string(), read_only: None }), ..Default::default() }]),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    #[test]
    fn a_pvc_referenced_by_a_live_pod_is_in_use() {
        let pods = HashMap::from([("default/p".to_string(), pod_with_pvc("default", "c1"))]);
        assert!(pvc_in_use("default", "c1", &pods));
        assert!(!pvc_in_use("default", "c2", &pods));
    }

    #[test]
    fn a_pvc_referenced_only_by_a_terminating_pod_is_not_in_use() {
        let mut pod = pod_with_pvc("default", "c1");
        pod.metadata.deletion_timestamp = Some(crate::k8s_time::from_chrono(crate::k8s_time::now()));
        let pods = HashMap::from([("default/p".to_string(), pod)]);
        assert!(!pvc_in_use("default", "c1", &pods));
    }
}
