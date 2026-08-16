//! statefulset-controller (Group E, workload controllers): the last of the
//! four workload controllers. Unlike ReplicaSet/DaemonSet, Pods have a
//! stable, ordinal identity (`{name}-0`, `{name}-1`, ...) — this file
//! creates/deletes/updates them directly, no ReplicaSet involved, in
//! ordinal order rather than all at once.
//!
//! # Scope of this slice
//!
//! **`podManagementPolicy: OrderedReady` (the default) is enforced
//! literally: one ordinal in flight at a time.** Scale-up creates only the
//! lowest missing ordinal per reconcile (the next one is created once this
//! one's own watch event shows it Ready); scale-down deletes only the
//! highest excess ordinal per reconcile (the next is deleted once this
//! one's own watch event shows it gone). `Parallel` creates/deletes every
//! missing/excess ordinal in the same reconcile — both match upstream's
//! two real policies.
//!
//! **Rolling update is sequential, one ordinal at a time, highest-first
//! down to `partition`** (upstream's real default absent the
//! alpha `MaxUnavailableStatefulSet` feature gate, which this file doesn't
//! implement — `maxUnavailable` in `spec.updateStrategy.rollingUpdate` is
//! read as if it were always effectively 1). `partition` itself *is*
//! honored: ordinals below `partition` are left on whatever revision they
//! already have, the real mechanism a canary/staged rollout depends on.
//!
//! **`volumeClaimTemplates`**: a PVC is created per template per ordinal
//! (`{claim}-{statefulset}-{ordinal}`) the first time that ordinal is
//! created, with **no owner reference** — matching upstream's *default*
//! `persistentVolumeClaimRetentionPolicy` (`Retain` on both delete and
//! scale-down, i.e. "PVCs outlive the StatefulSet/Pod that made them").
//! **The `Delete` retention policy is not implemented** — a PVC is never
//! deleted by this controller regardless of what the policy field says;
//! that field is read nowhere. This is silent-safe (a real difference
//! from the *default*, un-set-field case is impossible — a PVC never
//! being deleted by this controller is the *actual* default upstream
//! behavior too), but a StatefulSet that explicitly asks for `Delete`
//! won't get it.
//!
//! **No `ControllerRevision` history / rollback**, same simplification
//! `deployment.rs`/`daemon_set.rs` document — a single current-template
//! hash, no stored past revisions.
//!
//! **No PVC-bound readiness gate** — a Pod is created as soon as its PVCs
//! exist, not once they're `Bound` (PV binding is Group G,
//! `persistentvolume-binder-controller`, not implemented — see
//! `docs/CONTROLLER_MANAGER.md`). Without dynamic provisioning or a
//! pre-created PV, the PVC stays `Pending` and the Pod stays unscheduled
//! for a volume reason, the same honestly-visible gap every other
//! PVC-dependent path in this project has today.

use anyhow::{Context, Result};
use futures::StreamExt;
use k8s_openapi::api::apps::v1::{StatefulSet, StatefulSetStatus};
use k8s_openapi::api::core::v1::{PersistentVolumeClaim, PersistentVolumeClaimVolumeSource, Pod, Volume};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use kube::api::{Api, Patch, PatchParams, PostParams};
use kube::runtime::watcher::Event;
use kube::{Client, ResourceExt};
use std::collections::{BTreeMap, BTreeSet, HashMap};

const POD_TEMPLATE_HASH_LABEL: &str = "pod-template-hash";

fn compute_template_hash<T: serde::Serialize>(template: &T) -> String {
    let bytes = serde_json::to_vec(template).unwrap_or_default();
    let mut hash: u64 = 0xcbf29ce484222325;
    for b in bytes {
        hash ^= b as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{hash:x}")
}

pub fn pod_name(sts_name: &str, ordinal: i32) -> String {
    format!("{sts_name}-{ordinal}")
}

fn ordinal_from_pod_name(sts_name: &str, pod_name: &str) -> Option<i32> {
    pod_name.strip_prefix(sts_name)?.strip_prefix('-')?.parse().ok()
}

/// `OrderedReady`: only the single lowest missing ordinal below `desired`
/// (nothing to create if any lower ordinal isn't already Ready — wait for
/// it first). `Parallel`: every missing ordinal at once.
pub fn ordinals_to_create(desired: i32, existing: &BTreeSet<i32>, ready: &BTreeSet<i32>, parallel: bool) -> Vec<i32> {
    let missing: Vec<i32> = (0..desired).filter(|o| !existing.contains(o)).collect();
    if parallel {
        return missing;
    }
    match missing.first() {
        Some(&lowest) => {
            let all_lower_ready = (0..lowest).all(|o| ready.contains(&o));
            if all_lower_ready { vec![lowest] } else { vec![] }
        }
        None => vec![],
    }
}

/// `OrderedReady`: only the single highest excess ordinal (`>= desired`).
/// `Parallel`: every excess ordinal at once.
pub fn ordinals_to_delete(desired: i32, existing: &BTreeSet<i32>, parallel: bool) -> Vec<i32> {
    let excess: Vec<i32> = existing.iter().copied().filter(|&o| o >= desired).collect();
    if parallel {
        return excess;
    }
    excess.iter().max().copied().into_iter().collect()
}

/// The single highest out-of-date ordinal in `[partition, desired)` to
/// replace this reconcile — sequential, one at a time, same reasoning as
/// `ordinals_to_create`/`ordinals_to_delete`.
pub fn ordinal_to_update(desired: i32, partition: i32, pod_hashes: &BTreeMap<i32, String>, current_hash: &str) -> Option<i32> {
    (partition.max(0)..desired).rev().find(|o| pod_hashes.get(o).map(|h| h.as_str()) != Some(current_hash))
}

fn owner_reference(sts: &StatefulSet) -> k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference {
    k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference {
        api_version: "apps/v1".to_string(),
        kind: "StatefulSet".to_string(),
        name: sts.name_any(),
        uid: sts.uid().unwrap_or_default(),
        controller: Some(true),
        block_owner_deletion: Some(true),
        ..Default::default()
    }
}

fn owned_by(pod: &Pod, sts_uid: &str) -> bool {
    pod.metadata.owner_references.as_ref().into_iter().flatten().any(|o| o.controller == Some(true) && o.uid == sts_uid)
}

fn pod_ready(pod: &Pod) -> bool {
    pod.status.as_ref().and_then(|s| s.conditions.as_ref()).into_iter().flatten().any(|c| c.type_ == "Ready" && c.status == "True")
}

fn pod_hash(pod: &Pod) -> Option<&str> {
    pod.metadata.labels.as_ref().and_then(|l| l.get(POD_TEMPLATE_HASH_LABEL)).map(|s| s.as_str())
}

/// The generated per-ordinal PVC's own object name — shared by `build_pvc`
/// (which creates it) and `build_pod` (which must reference the exact same
/// name in its injected volume) so the two can never drift apart.
fn pvc_name_for_ordinal(claim_name: &str, sts_name: &str, ordinal: i32) -> String {
    format!("{claim_name}-{sts_name}-{ordinal}")
}

fn build_pod(sts: &StatefulSet, ordinal: i32, hash: &str) -> Option<Pod> {
    let spec = sts.spec.as_ref()?;
    let mut pod_spec = spec.template.spec.clone()?;
    // Real StatefulSet semantics: a `volumeClaimTemplate` named e.g. "www"
    // means containers mount a volume literally named "www" — the PVC
    // object backing it has a different, generated name
    // (`pvc_name_for_ordinal`), but the Volume's own `.name` is the
    // template's name, matching what `volumeMounts` in the template
    // reference. Without this the apiserver rejects the Pod outright:
    // every volumeMount has no corresponding `spec.volumes` entry.
    let sts_name = sts.name_any();
    for template in spec.volume_claim_templates.iter().flatten() {
        let Some(claim_name) = template.metadata.name.as_ref() else { continue };
        pod_spec.volumes.get_or_insert_with(Vec::new).push(Volume {
            name: claim_name.clone(),
            persistent_volume_claim: Some(PersistentVolumeClaimVolumeSource {
                claim_name: pvc_name_for_ordinal(claim_name, &sts_name, ordinal),
                read_only: None,
            }),
            ..Default::default()
        });
    }
    let mut labels = spec.template.metadata.as_ref().and_then(|m| m.labels.clone()).unwrap_or_default();
    labels.insert(POD_TEMPLATE_HASH_LABEL.to_string(), hash.to_string());
    let annotations = spec.template.metadata.as_ref().and_then(|m| m.annotations.clone());
    Some(Pod {
        metadata: ObjectMeta {
            name: Some(pod_name(&sts_name, ordinal)),
            namespace: sts.namespace(),
            labels: Some(labels),
            annotations,
            owner_references: Some(vec![owner_reference(sts)]),
            ..Default::default()
        },
        spec: Some(pod_spec),
        ..Default::default()
    })
}

fn build_pvc(sts: &StatefulSet, template: &PersistentVolumeClaim, ordinal: i32) -> Option<PersistentVolumeClaim> {
    let claim_name = template.metadata.name.as_ref()?;
    Some(PersistentVolumeClaim {
        metadata: ObjectMeta {
            name: Some(pvc_name_for_ordinal(claim_name, &sts.name_any(), ordinal)),
            namespace: sts.namespace(),
            labels: template.metadata.labels.clone(),
            annotations: template.metadata.annotations.clone(),
            // No owner reference — see module doc on the default Retain
            // retention policy.
            ..Default::default()
        },
        spec: template.spec.clone(),
        ..Default::default()
    })
}

async fn ensure_pvcs_for_ordinal(client: &Client, namespace: &str, sts: &StatefulSet, ordinal: i32) {
    let Some(templates) = sts.spec.as_ref().and_then(|s| s.volume_claim_templates.as_ref()) else { return };
    let pvc_api: Api<PersistentVolumeClaim> = Api::namespaced(client.clone(), namespace);
    for template in templates {
        let Some(pvc) = build_pvc(sts, template, ordinal) else { continue };
        let name = pvc.name_any();
        match pvc_api.get_opt(&name).await {
            Ok(Some(_)) => continue, // already exists — never recreated/deleted by this controller
            Ok(None) => {}
            Err(e) => {
                tracing::warn!(namespace = %namespace, pvc = %name, error = ?e, "failed to check for existing StatefulSet PVC");
                continue;
            }
        }
        if let Err(e) = pvc_api.create(&PostParams::default(), &pvc).await {
            if !matches!(&e, kube::Error::Api(status) if status.is_already_exists()) {
                tracing::warn!(namespace = %namespace, pvc = %name, error = ?e, "failed to create StatefulSet PVC");
            }
        }
    }
}

async fn reconcile_stateful_set(client: &Client, namespace: &str, name: &str, pod_cache: &HashMap<String, Pod>) {
    let sts_api: Api<StatefulSet> = Api::namespaced(client.clone(), namespace);
    let pod_api: Api<Pod> = Api::namespaced(client.clone(), namespace);

    let sts = match sts_api.get_opt(name).await {
        Ok(Some(sts)) => sts,
        Ok(None) => return,
        Err(e) => {
            tracing::warn!(namespace = %namespace, statefulset = %name, error = ?e, "failed to read StatefulSet for reconcile");
            return;
        }
    };
    let Some(sts_uid) = sts.uid() else { return };
    let Some(spec) = sts.spec.as_ref() else { return };
    let desired = spec.replicas.unwrap_or(1);
    let hash = compute_template_hash(&spec.template);
    let parallel = spec.pod_management_policy.as_deref() == Some("Parallel");

    let owned: Vec<&Pod> =
        pod_cache.values().filter(|p| p.namespace().as_deref() == Some(namespace)).filter(|p| owned_by(p, &sts_uid)).collect();
    let live: Vec<&&Pod> = owned.iter().filter(|p| p.metadata.deletion_timestamp.is_none()).collect();

    let mut by_ordinal: BTreeMap<i32, &Pod> = BTreeMap::new();
    for p in &live {
        if let Some(o) = ordinal_from_pod_name(name, &p.name_any()) {
            by_ordinal.insert(o, p);
        }
    }
    let existing: BTreeSet<i32> = by_ordinal.keys().copied().collect();
    let ready: BTreeSet<i32> = by_ordinal.iter().filter(|(_, p)| pod_ready(p)).map(|(o, _)| *o).collect();

    for ordinal in ordinals_to_create(desired, &existing, &ready, parallel) {
        ensure_pvcs_for_ordinal(client, namespace, &sts, ordinal).await;
        let Some(pod) = build_pod(&sts, ordinal, &hash) else {
            tracing::warn!(namespace = %namespace, statefulset = %name, "StatefulSet has no pod template — cannot create Pods");
            break;
        };
        match pod_api.create(&PostParams::default(), &pod).await {
            Ok(_) => {}
            Err(kube::Error::Api(ref status)) if status.is_already_exists() => {}
            Err(e) => tracing::warn!(namespace = %namespace, statefulset = %name, ordinal, error = ?e, "failed to create StatefulSet Pod"),
        }
    }

    for ordinal in ordinals_to_delete(desired, &existing, parallel) {
        if let Some(pod) = by_ordinal.get(&ordinal) {
            if let Err(e) = pod_api.delete(&pod.name_any(), &Default::default()).await {
                tracing::warn!(namespace = %namespace, statefulset = %name, ordinal, error = ?e, "failed to delete excess StatefulSet Pod");
            }
        }
    }

    // Rolling update — only once every ordinal below `desired` exists, so
    // scale-up and update never race each other in the same reconcile.
    if existing.len() as i32 >= desired {
        let partition = spec.update_strategy.as_ref().and_then(|s| s.rolling_update.as_ref()).and_then(|r| r.partition).unwrap_or(0);
        let pod_hashes: BTreeMap<i32, String> =
            by_ordinal.iter().filter_map(|(o, p)| pod_hash(p).map(|h| (*o, h.to_string()))).collect();
        if let Some(ordinal) = ordinal_to_update(desired, partition, &pod_hashes, &hash) {
            if let Some(pod) = by_ordinal.get(&ordinal) {
                if let Err(e) = pod_api.delete(&pod.name_any(), &Default::default()).await {
                    tracing::warn!(namespace = %namespace, statefulset = %name, ordinal, error = ?e, "failed to delete outdated StatefulSet Pod for rolling update");
                }
            }
        }
    }

    let ready_count = ready.len() as i32;
    let updated_count = by_ordinal.values().filter(|p| pod_hash(p) == Some(hash.as_str())).count() as i32;
    let status = StatefulSetStatus {
        replicas: existing.len() as i32,
        ready_replicas: Some(ready_count),
        available_replicas: Some(ready_count), // minReadySeconds not tracked, see module doc pattern elsewhere
        // current_revision == update_revision always in this simplified
        // model (see module doc: no ControllerRevision history, one hash
        // only) — currentReplicas is just "not yet on the new template".
        current_replicas: Some((existing.len() as i32 - updated_count).max(0)),
        updated_replicas: Some(updated_count),
        update_revision: Some(hash.clone()),
        current_revision: Some(hash.clone()),
        observed_generation: sts.metadata.generation,
        ..Default::default()
    };
    if sts.status.as_ref() != Some(&status) {
        let patch = serde_json::json!({ "status": status });
        if let Err(e) = sts_api.patch_status(name, &PatchParams::default(), &Patch::Merge(&patch)).await {
            tracing::warn!(namespace = %namespace, statefulset = %name, error = ?e, "failed to patch StatefulSet status");
        }
    }
}

fn ns_of<K: ResourceExt>(obj: &K) -> String {
    obj.namespace().unwrap_or_default()
}

pub async fn run(client: Client, _cfg: &crate::config::Config) -> Result<()> {
    let mut pods: HashMap<String, Pod> = HashMap::new();
    let mut stateful_sets: std::collections::HashSet<(String, String)> = std::collections::HashSet::new();

    let pod_api: Api<Pod> = Api::all(client.clone());
    let sts_api: Api<StatefulSet> = Api::all(client.clone());

    for p in pod_api.list(&Default::default()).await.context("listing Pods to seed statefulset-controller")?.items {
        pods.insert(format!("{}/{}", ns_of(&p), p.name_any()), p);
    }
    for sts in sts_api.list(&Default::default()).await.context("listing StatefulSets to seed statefulset-controller")?.items {
        let ns = ns_of(&sts);
        let name = sts.name_any();
        stateful_sets.insert((ns.clone(), name.clone()));
        reconcile_stateful_set(&client, &ns, &name, &pods).await;
    }

    let mut pod_stream = crate::watch::watch_pods(&client);
    let mut sts_stream = crate::watch::watch_stateful_sets(&client);

    loop {
        tokio::select! {
            ev = pod_stream.next() => {
                match ev {
                    Some(Ok(Event::Apply(pod))) | Some(Ok(Event::InitApply(pod))) => {
                        let ns = ns_of(&pod);
                        pods.insert(format!("{ns}/{}", pod.name_any()), pod);
                        for (s_ns, s_name) in stateful_sets.iter().filter(|(n, _)| *n == ns) {
                            reconcile_stateful_set(&client, s_ns, s_name, &pods).await;
                        }
                    }
                    Some(Ok(Event::Delete(pod))) => {
                        let ns = ns_of(&pod);
                        pods.remove(&format!("{ns}/{}", pod.name_any()));
                        for (s_ns, s_name) in stateful_sets.iter().filter(|(n, _)| *n == ns) {
                            reconcile_stateful_set(&client, s_ns, s_name, &pods).await;
                        }
                    }
                    Some(Ok(Event::Init | Event::InitDone)) => {}
                    Some(Err(e)) => tracing::warn!(error = ?e, "pod watch error in statefulset-controller"),
                    None => return Ok(()),
                }
            }
            ev = sts_stream.next() => {
                match ev {
                    Some(Ok(Event::Apply(sts))) | Some(Ok(Event::InitApply(sts))) => {
                        let ns = ns_of(&sts);
                        let name = sts.name_any();
                        stateful_sets.insert((ns.clone(), name.clone()));
                        reconcile_stateful_set(&client, &ns, &name, &pods).await;
                    }
                    Some(Ok(Event::Delete(sts))) => {
                        stateful_sets.remove(&(ns_of(&sts), sts.name_any()));
                    }
                    Some(Ok(Event::Init | Event::InitDone)) => {}
                    Some(Err(e)) => tracing::warn!(error = ?e, "statefulset watch error in statefulset-controller"),
                    None => return Ok(()),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ordered_ready_creates_only_the_lowest_missing_ordinal() {
        let existing = BTreeSet::from([0]);
        let ready = BTreeSet::from([0]);
        assert_eq!(ordinals_to_create(3, &existing, &ready, false), vec![1]);
    }

    #[test]
    fn ordered_ready_waits_for_lower_ordinals_before_creating_the_next() {
        let existing = BTreeSet::from([0]);
        let ready = BTreeSet::new(); // 0 exists but isn't Ready yet
        assert_eq!(ordinals_to_create(3, &existing, &ready, false), Vec::<i32>::new());
    }

    #[test]
    fn parallel_creates_every_missing_ordinal_at_once() {
        let existing = BTreeSet::new();
        let ready = BTreeSet::new();
        assert_eq!(ordinals_to_create(3, &existing, &ready, true), vec![0, 1, 2]);
    }

    #[test]
    fn ordered_ready_deletes_only_the_highest_excess_ordinal() {
        let existing = BTreeSet::from([0, 1, 2]);
        assert_eq!(ordinals_to_delete(1, &existing, false), vec![2]);
    }

    #[test]
    fn parallel_deletes_every_excess_ordinal_at_once() {
        let existing = BTreeSet::from([0, 1, 2]);
        assert_eq!(ordinals_to_delete(1, &existing, true), vec![1, 2]);
    }

    #[test]
    fn nothing_to_delete_when_under_desired() {
        let existing = BTreeSet::from([0]);
        assert_eq!(ordinals_to_delete(3, &existing, false), Vec::<i32>::new());
    }

    #[test]
    fn rolling_update_replaces_highest_ordinal_first() {
        let hashes = BTreeMap::from([(0, "old".to_string()), (1, "old".to_string()), (2, "old".to_string())]);
        assert_eq!(ordinal_to_update(3, 0, &hashes, "new"), Some(2));
    }

    #[test]
    fn rolling_update_respects_the_partition() {
        let hashes = BTreeMap::from([(0, "old".to_string()), (1, "old".to_string()), (2, "old".to_string())]);
        // Only ordinal 2 (>= partition 2) may be touched.
        assert_eq!(ordinal_to_update(3, 2, &hashes, "new"), Some(2));
        let hashes2 = BTreeMap::from([(0, "old".to_string()), (1, "old".to_string()), (2, "new".to_string())]);
        assert_eq!(ordinal_to_update(3, 2, &hashes2, "new"), None);
    }

    #[test]
    fn ordinal_parsing_from_pod_name() {
        assert_eq!(ordinal_from_pod_name("web", "web-0"), Some(0));
        assert_eq!(ordinal_from_pod_name("web", "web-12"), Some(12));
        assert_eq!(ordinal_from_pod_name("web", "other-0"), None);
    }

    fn sts_with_pvc_template() -> StatefulSet {
        use k8s_openapi::api::apps::v1::StatefulSetSpec;
        use k8s_openapi::api::core::v1::{Container, PodSpec, PodTemplateSpec, VolumeMount};
        StatefulSet {
            metadata: ObjectMeta { name: Some("web".to_string()), namespace: Some("default".to_string()), ..Default::default() },
            spec: Some(StatefulSetSpec {
                service_name: Some("web".to_string()),
                template: PodTemplateSpec {
                    metadata: None,
                    spec: Some(PodSpec {
                        containers: vec![Container {
                            name: "app".to_string(),
                            volume_mounts: Some(vec![VolumeMount {
                                name: "www".to_string(),
                                mount_path: "/data".to_string(),
                                ..Default::default()
                            }]),
                            ..Default::default()
                        }],
                        ..Default::default()
                    }),
                },
                volume_claim_templates: Some(vec![PersistentVolumeClaim {
                    metadata: ObjectMeta { name: Some("www".to_string()), ..Default::default() },
                    ..Default::default()
                }]),
                ..Default::default()
            }),
            status: None,
        }
    }

    // The actual bug CodeRabbit caught in PR #29: build_pod cloned the
    // template PodSpec verbatim, so a volumeMount referencing a
    // volumeClaimTemplate had no matching spec.volumes entry and the
    // apiserver rejected every such Pod outright.
    #[test]
    fn build_pod_injects_a_volume_for_each_pvc_template_matching_build_pvcs_name() {
        let sts = sts_with_pvc_template();
        let pod = build_pod(&sts, 0, "hash1").expect("build_pod should produce a Pod");
        let volumes = pod.spec.as_ref().and_then(|s| s.volumes.as_ref()).expect("Pod must have injected volumes");
        assert_eq!(volumes.len(), 1);
        let vol = &volumes[0];
        // Volume name matches the volumeMount in the template ("www"), not
        // the generated PVC's own object name.
        assert_eq!(vol.name, "www");
        let claim = vol.persistent_volume_claim.as_ref().expect("volume must be PVC-backed");

        let template = sts.spec.as_ref().unwrap().volume_claim_templates.as_ref().unwrap().first().unwrap();
        let pvc = build_pvc(&sts, template, 0).expect("build_pvc should produce a PVC");
        // The two must never drift: the volume's claim_name is exactly the
        // PVC object build_pvc actually creates.
        assert_eq!(claim.claim_name, pvc.metadata.name.unwrap());
    }
}
