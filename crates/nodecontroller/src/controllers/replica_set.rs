//! replicaset-controller (Group E, workload controllers): ensures a
//! ReplicaSet's `spec.replicas` Pods exist, matching its selector and pod
//! template. Pure event — a ReplicaSet's actual vs. desired Pod count
//! changes exactly when a Pod or the ReplicaSet itself changes, nothing
//! to poll.
//!
//! The foundation Group E is built on: `deployment-controller` manages
//! ReplicaSets, not Pods directly — without this file, nothing that
//! creates a ReplicaSet (directly, or via a Deployment once that lands)
//! ever produces a Pod.
//!
//! # Scope of this slice
//!
//! **Only manages Pods it owns** (an `ownerReference` back to this exact
//! ReplicaSet's UID, `controller: true`) — no *adoption* of pre-existing,
//! unowned Pods that happen to match the selector. Upstream adopts these;
//! this is a real, deliberate simplification for a first slice, not
//! something silently missed — the common case (create the ReplicaSet,
//! then it creates its own Pods) is unaffected, and adoption only matters
//! for hand-crafted edge cases (a Pod created directly, then a matching
//! ReplicaSet added later).
//!
//! **Deletion ranking is simplified**: not-Ready Pods are removed before
//! Ready ones when scaling down, then by name for determinism. Upstream
//! ranks by several more criteria (pod-deletion-cost annotation, restart
//! count, node spread, creation time) — a real difference for *which*
//! survivor a scale-down picks, never for *how many*.
//!
//! **`status.availableReplicas` mirrors `readyReplicas`** — `minReadySeconds`
//! (a Pod must stay Ready for N seconds before counting as "available") is
//! not tracked; this crate has no poll-worthy per-Pod timer for it yet, and
//! most ReplicaSets never set it (default `0`, where the two are identical
//! anyway).
//!
//! **A deleted ReplicaSet's Pods are not cleaned up here** — that's
//! owner-reference cascade deletion, `garbage-collector-controller`
//! (Group D), still not implemented (see that file's own module doc for
//! why). A `kubectl delete replicaset` leaves its Pods running until
//! something else removes them, same as `kubectl delete service` did
//! before Group D's minimum slice — a real, known gap, not new to this file.

use anyhow::Result;
use crate::workqueue::KeyedWorkQueue;
use futures::StreamExt;
use k8s_openapi::api::apps::v1::{ReplicaSet, ReplicaSetStatus};
use k8s_openapi::api::core::v1::Pod;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{LabelSelector, ObjectMeta, OwnerReference};
use kube::api::{Api, Patch, PatchParams, PostParams};
use kube::runtime::watcher::Event;
use kube::{Client, ResourceExt};
use rand::Rng;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::time::{Duration, Instant};

const CREATION_EXPECTATION_TIMEOUT: Duration = Duration::from_secs(30);
const STATUS_PATCH_ATTEMPTS: usize = 3;
const STATUS_PATCH_BACKOFF: Duration = Duration::from_millis(25);

#[derive(Debug)]
struct PendingCreates {
    names: HashSet<String>,
    oldest: Instant,
}

type Expectations = HashMap<(String, String), PendingCreates>;

/// Does `selector` match `labels`? `matchLabels` (all present, equal) AND
/// every `matchExpressions` entry (`In`/`NotIn`/`Exists`/`DoesNotExist`).
/// A selector with neither set is empty and — the same rule
/// `endpoint_slice::selector_matches` uses — matches nothing, not
/// everything: an object with no real selector isn't this controller's
/// business.
pub fn label_selector_matches(selector: &LabelSelector, labels: &BTreeMap<String, String>) -> bool {
    let has_match_labels = selector.match_labels.is_some();
    let has_match_expressions = selector
        .match_expressions
        .as_ref()
        .is_some_and(|e| !e.is_empty());
    if !has_match_labels && !has_match_expressions {
        return false;
    }
    if let Some(match_labels) = &selector.match_labels {
        if !match_labels.iter().all(|(k, v)| labels.get(k) == Some(v)) {
            return false;
        }
    }
    if let Some(exprs) = &selector.match_expressions {
        for e in exprs {
            let satisfied = match e.operator.as_str() {
                "In" => e
                    .values
                    .as_ref()
                    .is_some_and(|vs| labels.get(&e.key).is_some_and(|v| vs.contains(v))),
                "NotIn" => !e
                    .values
                    .as_ref()
                    .is_some_and(|vs| labels.get(&e.key).is_some_and(|v| vs.contains(v))),
                "Exists" => labels.contains_key(&e.key),
                "DoesNotExist" => !labels.contains_key(&e.key),
                _ => false, // an operator we don't recognize fails closed, not open
            };
            if !satisfied {
                return false;
            }
        }
    }
    true
}

/// How many Pods to create (positive) given `desired` and `current` —
/// pure arithmetic, split out because it's the one number a scale
/// operation actually hinges on.
pub fn pods_to_create(desired: i32, current: usize) -> i32 {
    (desired - current as i32).max(0)
}

pub fn pods_to_delete_count(desired: i32, current: usize) -> usize {
    (current as i32 - desired).max(0) as usize
}

/// The minimal projection deletion-ranking needs — pure and independent
/// of the real `Pod` type, the same "project down to what the decision
/// needs" discipline `nodescheduler`'s cache layer uses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeletionCandidate {
    pub name: String,
    pub ready: bool,
}

/// Picks which `excess` Pods to delete: not-Ready before Ready, then by
/// name for determinism (so a test — or an operator staring at two
/// otherwise-identical Pods — gets a reproducible answer, not whichever
/// order a `HashMap` happened to iterate in).
pub fn pods_to_delete(mut candidates: Vec<DeletionCandidate>, excess: usize) -> Vec<String> {
    candidates.sort_by(|a, b| a.ready.cmp(&b.ready).then_with(|| a.name.cmp(&b.name)));
    candidates
        .into_iter()
        .take(excess)
        .map(|c| c.name)
        .collect()
}

fn pod_ready(pod: &Pod) -> bool {
    pod.status
        .as_ref()
        .and_then(|s| s.conditions.as_ref())
        .into_iter()
        .flatten()
        .any(|c| c.type_ == "Ready" && c.status == "True")
}

fn owned_by(pod: &Pod, rs_uid: &str) -> bool {
    pod.metadata
        .owner_references
        .as_ref()
        .into_iter()
        .flatten()
        .any(|o| o.controller == Some(true) && o.uid == rs_uid)
}

fn owning_replicaset(pod: &Pod) -> Option<String> {
    pod.metadata
        .owner_references
        .as_ref()
        .into_iter()
        .flatten()
        .find(|o| o.controller == Some(true) && o.kind == "ReplicaSet")
        .map(|o| o.name.clone())
}

fn note_pod_event(expectations: &mut Expectations, pod: &Pod) {
    let Some(rs_name) = owning_replicaset(pod) else {
        return;
    };
    let key = (pod.namespace().unwrap_or_default(), rs_name);
    let Some(pending) = expectations.get_mut(&key) else {
        return;
    };
    pending.names.remove(&pod.name_any());
    if pending.names.is_empty() {
        expectations.remove(&key);
    }
}

fn random_suffix() -> String {
    // Same character set upstream's own name generator uses (no vowels,
    // '0'/'1'/'l'/'o' — nothing that reads ambiguously in a terminal),
    // just a plain independent implementation rather than a shared dep.
    const CHARSET: &[u8] = b"bcdfghjklmnpqrstvwxz2456789";
    let mut rng = rand::thread_rng();
    (0..5)
        .map(|_| CHARSET[rng.gen_range(0..CHARSET.len())] as char)
        .collect()
}

fn owner_reference(rs: &ReplicaSet) -> OwnerReference {
    OwnerReference {
        api_version: "apps/v1".to_string(),
        kind: "ReplicaSet".to_string(),
        name: rs.name_any(),
        uid: rs.uid().unwrap_or_default(),
        controller: Some(true),
        block_owner_deletion: Some(true),
        ..Default::default()
    }
}

fn build_pod(rs: &ReplicaSet, name: &str) -> Option<Pod> {
    let template = rs.spec.as_ref()?.template.clone()?;
    let labels = template.metadata.as_ref().and_then(|m| m.labels.clone());
    let annotations = template
        .metadata
        .as_ref()
        .and_then(|m| m.annotations.clone());
    Some(Pod {
        metadata: ObjectMeta {
            name: Some(name.to_string()),
            namespace: rs.namespace(),
            labels,
            annotations,
            owner_references: Some(vec![owner_reference(rs)]),
            ..Default::default()
        },
        spec: template.spec,
        ..Default::default()
    })
}

async fn reconcile_replica_set(
    client: &Client,
    rs: &ReplicaSet,
    pod_cache: &HashMap<String, Pod>,
    expectations: &mut Expectations,
) {
    let namespace = ns_of(rs);
    let name = rs.name_any();
    let rs_api: Api<ReplicaSet> = Api::namespaced(client.clone(), &namespace);
    let pod_api: Api<Pod> = Api::namespaced(client.clone(), &namespace);
    let Some(rs_uid) = rs.uid() else { return };
    let desired = rs.spec.as_ref().and_then(|s| s.replicas).unwrap_or(1);

    let owned: Vec<&Pod> = pod_cache
        .values()
        .filter(|p| p.namespace().as_deref() == Some(namespace.as_str()))
        .filter(|p| owned_by(p, &rs_uid))
        .collect();
    let live: Vec<&&Pod> = owned
        .iter()
        .filter(|p| p.metadata.deletion_timestamp.is_none())
        .collect();

    let expectation_key = (namespace.to_string(), name.to_string());
    if expectations
        .get(&expectation_key)
        .is_some_and(|pending| pending.oldest.elapsed() >= CREATION_EXPECTATION_TIMEOUT)
    {
        expectations.remove(&expectation_key);
    }
    let in_flight = expectations
        .get(&expectation_key)
        .map(|pending| pending.names.len())
        .unwrap_or(0);
    let to_create = pods_to_create(desired, live.len() + in_flight);
    for _ in 0..to_create {
        let Some(pod_name) = rs
            .metadata
            .name
            .as_ref()
            .map(|n| format!("{n}-{}", random_suffix()))
        else {
            break;
        };
        let Some(pod) = build_pod(rs, &pod_name) else {
            tracing::warn!(namespace = %namespace, replicaset = %name, "ReplicaSet has no pod template — cannot create Pods");
            break;
        };
        match pod_api.create(&PostParams::default(), &pod).await {
            Ok(_) => {
                let pending = expectations
                    .entry(expectation_key.clone())
                    .or_insert_with(|| PendingCreates {
                        names: HashSet::new(),
                        oldest: Instant::now(),
                    });
                pending.names.insert(pod_name);
            }
            Err(e) => {
                tracing::warn!(namespace = %namespace, replicaset = %name, pod = %pod_name, error = ?e, "failed to create Pod for ReplicaSet");
            }
        }
    }

    let to_delete = pods_to_delete_count(desired, live.len());
    if to_delete > 0 {
        let candidates: Vec<DeletionCandidate> = live
            .iter()
            .map(|p| DeletionCandidate {
                name: p.name_any(),
                ready: pod_ready(p),
            })
            .collect();
        for pod_name in pods_to_delete(candidates, to_delete) {
            if let Err(e) = pod_api.delete(&pod_name, &Default::default()).await {
                tracing::warn!(namespace = %namespace, replicaset = %name, pod = %pod_name, error = ?e, "failed to delete excess Pod for ReplicaSet");
            }
        }
    }

    let ready = owned.iter().filter(|p| pod_ready(p)).count() as i32;
    let status = ReplicaSetStatus {
        replicas: owned.len() as i32,
        ready_replicas: Some(ready),
        available_replicas: Some(ready), // see module doc: minReadySeconds not tracked
        ..Default::default()
    };
    if rs.status.as_ref() != Some(&status) {
        let patch = serde_json::json!({ "status": status });
        for attempt in 0..STATUS_PATCH_ATTEMPTS {
            match rs_api
                .patch_status(&name, &PatchParams::default(), &Patch::Merge(&patch))
                .await
            {
                Ok(_) => break,
                Err(e) if attempt + 1 < STATUS_PATCH_ATTEMPTS => {
                    // Deployment reconciliation and ReplicaSet status
                    // bookkeeping can legitimately update the same object
                    // at startup. A lost optimistic-concurrency race must
                    // not strand the Deployment at availableReplicas=0.
                    tracing::debug!(
                        namespace = %namespace,
                        replicaset = %name,
                        attempt = attempt + 1,
                        error = ?e,
                        "ReplicaSet status patch conflicted; retrying"
                    );
                    tokio::time::sleep(STATUS_PATCH_BACKOFF * (attempt as u32 + 1)).await;
                }
                Err(e) => {
                    tracing::warn!(namespace = %namespace, replicaset = %name, error = ?e, "failed to patch ReplicaSet status");
                    break;
                }
            }
        }
    }
}

fn ns_of<K: ResourceExt>(obj: &K) -> String {
    obj.namespace().unwrap_or_default()
}

pub async fn run(client: Client, _cfg: &crate::config::Config) -> Result<()> {
    let mut pods: HashMap<String, Pod> = HashMap::new();
    let mut replica_sets: HashMap<String, ReplicaSet> = HashMap::new();
    let mut expectations: Expectations = HashMap::new();
    let queue: KeyedWorkQueue<String> = KeyedWorkQueue::default();

    let mut pod_stream = crate::watch::watch_pods(&client);
    let mut rs_stream = crate::watch::watch_replica_sets(&client);

    loop {
        tokio::select! {
            ev = pod_stream.next() => {
                match ev {
                    Some(Ok(Event::Apply(pod))) | Some(Ok(Event::InitApply(pod))) => {
                        let ns = ns_of(&pod);
                        note_pod_event(&mut expectations, &pod);
                        pods.insert(format!("{ns}/{}", pod.name_any()), pod);
                        for (key, _rs) in replica_sets.iter().filter(|(_, rs)| ns_of(*rs) == ns) {
                            queue.enqueue(key.clone());
                        }
                    }
                    Some(Ok(Event::Delete(pod))) => {
                        let ns = ns_of(&pod);
                        note_pod_event(&mut expectations, &pod);
                        pods.remove(&format!("{ns}/{}", pod.name_any()));
                        for (key, _rs) in replica_sets.iter().filter(|(_, rs)| ns_of(*rs) == ns) {
                            queue.enqueue(key.clone());
                        }
                    }
                    Some(Ok(Event::Init | Event::InitDone)) => {}
                    Some(Err(e)) => tracing::warn!(error = ?e, "pod watch error in replicaset-controller"),
                    None => return Ok(()),
                }
            }
            ev = rs_stream.next() => {
                match ev {
                    Some(Ok(Event::Apply(rs))) | Some(Ok(Event::InitApply(rs))) => {
                        let ns = ns_of(&rs);
                        let name = rs.name_any();
                        let key = format!("{ns}/{name}");
                        replica_sets.insert(key.clone(), rs);
                        queue.enqueue(key);
                    }
                    Some(Ok(Event::Delete(rs))) => {
                        let ns = ns_of(&rs);
                        let name = rs.name_any();
                        replica_sets.remove(&format!("{ns}/{name}"));
                        expectations.remove(&(ns, name));
                    }
                    Some(Ok(Event::Init | Event::InitDone)) => {}
                    Some(Err(e)) => tracing::warn!(error = ?e, "replicaset watch error in replicaset-controller"),
                    None => return Ok(()),
                }
            }
            key = queue.pop() => {
                if let Some(rs) = replica_sets.get(&key).cloned() {
                    reconcile_replica_set(&client, &rs, &pods, &mut expectations).await;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn labels(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn an_empty_selector_matches_nothing() {
        assert!(!label_selector_matches(
            &LabelSelector::default(),
            &labels(&[("app", "web")])
        ));
    }

    #[test]
    fn match_labels_requires_every_pair_present() {
        let sel = LabelSelector {
            match_labels: Some(labels(&[("app", "web")])),
            ..Default::default()
        };
        assert!(label_selector_matches(
            &sel,
            &labels(&[("app", "web"), ("tier", "fe")])
        ));
        assert!(!label_selector_matches(&sel, &labels(&[("tier", "fe")])));
    }

    fn expr(
        key: &str,
        op: &str,
        values: &[&str],
    ) -> k8s_openapi::apimachinery::pkg::apis::meta::v1::LabelSelectorRequirement {
        k8s_openapi::apimachinery::pkg::apis::meta::v1::LabelSelectorRequirement {
            key: key.to_string(),
            operator: op.to_string(),
            values: if values.is_empty() {
                None
            } else {
                Some(values.iter().map(|s| s.to_string()).collect())
            },
        }
    }

    #[test]
    fn match_expressions_in_operator() {
        let sel = LabelSelector {
            match_expressions: Some(vec![expr("env", "In", &["prod", "staging"])]),
            ..Default::default()
        };
        assert!(label_selector_matches(&sel, &labels(&[("env", "prod")])));
        assert!(!label_selector_matches(&sel, &labels(&[("env", "dev")])));
    }

    #[test]
    fn match_expressions_exists_and_does_not_exist() {
        let exists = LabelSelector {
            match_expressions: Some(vec![expr("canary", "Exists", &[])]),
            ..Default::default()
        };
        assert!(label_selector_matches(&exists, &labels(&[("canary", "")])));
        assert!(!label_selector_matches(&exists, &labels(&[])));

        let absent = LabelSelector {
            match_expressions: Some(vec![expr("canary", "DoesNotExist", &[])]),
            ..Default::default()
        };
        assert!(!label_selector_matches(&absent, &labels(&[("canary", "")])));
        assert!(label_selector_matches(&absent, &labels(&[])));
    }

    #[test]
    fn scale_up_creates_the_shortfall() {
        assert_eq!(pods_to_create(3, 1), 2);
        assert_eq!(pods_to_create(1, 3), 0); // never negative
    }

    #[test]
    fn scale_down_counts_the_excess() {
        assert_eq!(pods_to_delete_count(1, 3), 2);
        assert_eq!(pods_to_delete_count(3, 1), 0);
    }

    #[test]
    fn deletion_prefers_not_ready_pods_first() {
        let candidates = vec![
            DeletionCandidate {
                name: "ready-a".to_string(),
                ready: true,
            },
            DeletionCandidate {
                name: "not-ready-b".to_string(),
                ready: false,
            },
        ];
        assert_eq!(
            pods_to_delete(candidates, 1),
            vec!["not-ready-b".to_string()]
        );
    }

    #[test]
    fn deletion_breaks_ties_by_name_for_determinism() {
        let candidates = vec![
            DeletionCandidate {
                name: "z-pod".to_string(),
                ready: true,
            },
            DeletionCandidate {
                name: "a-pod".to_string(),
                ready: true,
            },
        ];
        assert_eq!(pods_to_delete(candidates, 1), vec!["a-pod".to_string()]);
    }

    #[test]
    fn deletion_never_returns_more_than_asked_for() {
        let candidates = vec![
            DeletionCandidate {
                name: "a".to_string(),
                ready: true,
            },
            DeletionCandidate {
                name: "b".to_string(),
                ready: true,
            },
        ];
        assert_eq!(pods_to_delete(candidates, 5).len(), 2);
    }
}
