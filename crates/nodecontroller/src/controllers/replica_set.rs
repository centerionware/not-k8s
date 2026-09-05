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
// Keep one unavailable apiserver request from parking this controller's
// single event loop indefinitely. A timed-out reconcile is retried below.
const RECONCILE_TIMEOUT: Duration = Duration::from_secs(30);

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

/// Issue #454: a transient Pod-create/-delete failure (the incident this
/// closes: a webhook temporarily unavailable) used to be logged and
/// forgotten -- nothing about the ReplicaSet object itself changes when a
/// create attempt fails, so in this watch-driven, no-resync architecture
/// nothing would ever produce a future event to retry it. `true` here
/// means the caller should schedule a real retry.
async fn reconcile_replica_set(
    client: &Client,
    rs: &ReplicaSet,
    pod_cache: &HashMap<String, Pod>,
    expectations: &mut Expectations,
) -> bool {
    // A failed create/delete/status write does not itself mutate the
    // ReplicaSet, so a watch-driven controller would otherwise have no event
    // that can ever retry the desired state.
    let mut needs_retry = false;
    let namespace = ns_of(rs);
    let name = rs.name_any();
    let rs_api: Api<ReplicaSet> = Api::namespaced(client.clone(), &namespace);
    let pod_api: Api<Pod> = Api::namespaced(client.clone(), &namespace);
    let Some(rs_uid) = rs.uid() else { return needs_retry };
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
                needs_retry = true;
                tracing::warn!(namespace = %namespace, replicaset = %name, pod = %pod_name, error = ?e, "failed to create Pod for ReplicaSet; will retry");
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
                needs_retry = true;
                tracing::warn!(namespace = %namespace, replicaset = %name, pod = %pod_name, error = ?e, "failed to delete excess Pod for ReplicaSet; will retry");
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
        if let Err(e) = rs_api
            .patch_status(&name, &PatchParams::default(), &Patch::Merge(&patch))
            .await
        {
            needs_retry = true;
            tracing::warn!(namespace = %namespace, replicaset = %name, error = ?e, "failed to patch ReplicaSet status; will retry");
        }
    }
    needs_retry
}

fn ns_of<K: ResourceExt>(obj: &K) -> String {
    obj.namespace().unwrap_or_default()
}

/// How long to wait before retrying a ReplicaSet whose reconcile hit a
/// transient failure (issue #454) -- long enough that a truly transient
/// blip (a webhook briefly unavailable, an apiserver hiccup) has almost
/// certainly cleared, short enough that a real incident does not leave
/// replicas under- or over-provisioned for long. Fixed, not exponential:
/// this queue has no per-key backoff state to grow, and a bounded fixed
/// retry is a large improvement over the previous "never" on its own —
/// matching `namespace.rs`'s own `RETRY_PERIOD` precedent for the same
/// "honest, low-frequency, not a real resync loop" shape.
const RETRY_DELAY: Duration = Duration::from_secs(15);

/// Re-enqueue `key` after `RETRY_DELAY`, detached from the reconcile loop
/// so a slow retry can never block processing any other ReplicaSet or Pod
/// event in the meantime -- same reasoning nodelet's own `schedule_retry()`
/// documents for the identical shape.
fn schedule_retry(queue: &std::sync::Arc<KeyedWorkQueue<String>>, key: String) {
    let queue = queue.clone();
    tokio::spawn(async move {
        tokio::time::sleep(RETRY_DELAY).await;
        queue.enqueue(key);
    });
}

pub async fn run(client: Client, _cfg: &crate::config::Config) -> Result<()> {
    let mut pods: HashMap<String, Pod> = HashMap::new();
    let mut replica_sets: HashMap<String, ReplicaSet> = HashMap::new();
    let mut expectations: Expectations = HashMap::new();
    // Arc so a delayed retry (issue #454) can hold its own cheap clone from
    // a detached task, the same "outlive the failure" shape pods.rs's own
    // schedule_retry() already established for exactly this class of gap:
    // a transient failure (a webhook temporarily unavailable, live-observed
    // this session) whose object never changes, so nothing else would ever
    // produce a future watch event to retry it.
    let queue: std::sync::Arc<KeyedWorkQueue<String>> = std::sync::Arc::new(KeyedWorkQueue::default());

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
                    Some(Ok(Event::Init)) => {
                        pods.clear();
                        expectations.clear();
                        tracing::debug!(target: "nk_controller_trace", controller = "replicaset", "replaced Pod informer snapshot");
                    }
                    Some(Ok(Event::Delete(pod))) => {
                        let ns = ns_of(&pod);
                        note_pod_event(&mut expectations, &pod);
                        pods.remove(&format!("{ns}/{}", pod.name_any()));
                        for (key, _rs) in replica_sets.iter().filter(|(_, rs)| ns_of(*rs) == ns) {
                            queue.enqueue(key.clone());
                        }
                    }
                    Some(Ok(Event::InitDone)) => {}
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
                        tracing::debug!(target: "nk_controller_trace", controller = "replicaset", key = %key, "enqueued ReplicaSet event");
                        queue.enqueue(key);
                    }
                    Some(Ok(Event::Init)) => {
                        replica_sets.clear();
                        expectations.clear();
                        tracing::debug!(target: "nk_controller_trace", controller = "replicaset", "replaced ReplicaSet informer snapshot");
                    }
                    Some(Ok(Event::Delete(rs))) => {
                        let ns = ns_of(&rs);
                        let name = rs.name_any();
                        replica_sets.remove(&format!("{ns}/{name}"));
                        expectations.remove(&(ns, name));
                    }
                    Some(Ok(Event::InitDone)) => {}
                    Some(Err(e)) => tracing::warn!(error = ?e, "replicaset watch error in replicaset-controller"),
                    None => return Ok(()),
                }
            }
            key = queue.pop() => {
                if let Some(rs) = replica_sets.get(&key).cloned() {
                    tracing::debug!(target: "nk_controller_trace", controller = "replicaset", key = %key, "starting ReplicaSet reconcile");
                    let needs_retry = match tokio::time::timeout(
                        RECONCILE_TIMEOUT,
                        reconcile_replica_set(&client, &rs, &pods, &mut expectations),
                    ).await {
                        Ok(needs_retry) => needs_retry,
                        Err(_) => {
                            tracing::warn!(
                                target: "nk_controller_trace",
                                controller = "replicaset",
                                key = %key,
                                timeout_secs = RECONCILE_TIMEOUT.as_secs(),
                                "ReplicaSet reconcile timed out; retrying"
                            );
                            true
                        }
                    };
                    if needs_retry {
                        schedule_retry(&queue, key);
                    }
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
