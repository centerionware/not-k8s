//! daemonset-controller (Group E, workload controllers): ensures one Pod
//! of a DaemonSet's template runs on every eligible Node, directly — a
//! DaemonSet places Pods itself (`spec.nodeName` set at creation), the same
//! scheduler-bypass upstream uses, rather than going through
//! `nodescheduler`. No ReplicaSet involved; this file both decides
//! placement and creates the Pod.
//!
//! # Scope of this slice
//!
//! **Node eligibility is nodeSelector + taint/toleration only** — exact
//! `spec.template.spec.nodeSelector` map match (every key present and
//! equal; empty selector matches every Node) and every `NoSchedule`/
//! `NoExecute` Node taint tolerated by the Pod template's `tolerations`
//! (`PreferNoSchedule` is a scheduling hint upstream itself doesn't treat
//! as hard-excluding for DaemonSet, so it's ignored here too, correctly).
//! **No node affinity / pod affinity-antiaffinity evaluation** — a real
//! gap: `spec.template.spec.affinity.nodeAffinity` is not consulted at
//! all. **No implicit built-in toleration defaults** — upstream's
//! DaemonSet controller auto-tolerates several control-plane/lifecycle
//! taints (`node.kubernetes.io/not-ready`, `-unreachable`, etc.) unless
//! the DaemonSet spec's own toleration list overrides them; this file
//! requires the DaemonSet's own template to name every taint it needs to
//! tolerate explicitly. A DaemonSet whose template doesn't already
//! tolerate `node.kubernetes.io/not-ready`/`-unreachable` (most real
//! DaemonSet manifests do, since they need this exact behavior even
//! against upstream) simply won't schedule onto a not-ready Node here —
//! visible and fail-safe, not silently wrong.
//!
//! **Rolling update is simplified to a flat per-reconcile replacement
//! budget**: `maxUnavailable` outdated Pods (by `pod-template-hash`, same
//! mechanism `deployment.rs` uses) are deleted per reconcile call: the
//! next reconcile (triggered by that Pod's own delete event) creates the
//! replacement and, once it's healthy, considers the next batch. This
//! converges to the same end state as upstream's rolling update but
//! without upstream's `maxSurge` (create-before-delete) option — always
//! delete-then-create, so a DaemonSet's total Pod count can dip during a
//! rollout even if `maxSurge` was configured. `OnDelete` strategy (do
//! nothing until a human deletes the old Pod) is not implemented — every
//! DaemonSet here behaves as `RollingUpdate`.
//!
//! **No `ControllerRevision` history** — upstream stores each past
//! template as a `ControllerRevision` object for rollback; this file
//! computes the same kind of template hash `deployment.rs` does (see that
//! file's module doc for why it's not upstream's specific algorithm) but
//! keeps no history at all, current template only.
//!
//! **Status** mirrors what's cheap to compute from the cache:
//! `desiredNumberScheduled`/`currentNumberScheduled`/`numberReady`/
//! `numberAvailable`/`numberMisscheduled`/`updatedNumberScheduled`.
//! `numberAvailable` mirrors `numberReady` (`minReadySeconds` not tracked,
//! same simplification `replica_set.rs` documents).

use anyhow::Result;
use crate::workqueue::KeyedWorkQueue;
use futures::StreamExt;
use k8s_openapi::api::apps::v1::{DaemonSet, DaemonSetStatus};
use k8s_openapi::api::core::v1::{Node, Pod, Taint, Toleration};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{ObjectMeta, OwnerReference};
use kube::api::{Api, Patch, PatchParams, PostParams};
use kube::runtime::watcher::Event;
use kube::{Client, ResourceExt};
use std::collections::{BTreeMap, HashMap};

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

/// Bare equality match: every key in `selector` present in `labels` with
/// the same value. An empty selector matches every Node — the DaemonSet
/// convention (unlike a ReplicaSet/Service selector, where empty means
/// "matches nothing" — see `replica_set.rs`/`endpoint_slice.rs`).
pub fn node_selector_matches(
    selector: &BTreeMap<String, String>,
    labels: &BTreeMap<String, String>,
) -> bool {
    selector.iter().all(|(k, v)| labels.get(k) == Some(v))
}

fn toleration_matches(t: &Toleration, taint: &Taint) -> bool {
    let key_matches = match &t.key {
        None => true, // no key + Exists means "match everything"
        Some(k) => k == &taint.key,
    };
    if !key_matches {
        return false;
    }
    let effect_matches = t.effect.as_deref().is_none_or(|e| e == taint.effect);
    if !effect_matches {
        return false;
    }
    match t.operator.as_deref() {
        Some("Exists") | None if t.key.is_none() => true,
        Some("Exists") => true,
        _ => t.value.as_deref() == taint.value.as_deref(),
    }
}

/// Every `NoSchedule`/`NoExecute` taint on the Node must be tolerated —
/// `PreferNoSchedule` is a soft hint, not exclusionary (see module doc).
pub fn taints_tolerated(taints: &[Taint], tolerations: &[Toleration]) -> bool {
    taints
        .iter()
        .filter(|t| t.effect == "NoSchedule" || t.effect == "NoExecute")
        .all(|taint| tolerations.iter().any(|t| toleration_matches(t, taint)))
}

fn node_eligible(
    node: &Node,
    node_selector: &BTreeMap<String, String>,
    tolerations: &[Toleration],
) -> bool {
    if node.metadata.deletion_timestamp.is_some() {
        return false;
    }
    let labels = node.metadata.labels.clone().unwrap_or_default();
    if !node_selector_matches(node_selector, &labels) {
        return false;
    }
    let taints = node
        .spec
        .as_ref()
        .and_then(|s| s.taints.clone())
        .unwrap_or_default();
    taints_tolerated(&taints, tolerations)
}

/// A DaemonSet Pod's name is deterministic — `{daemonset}-{hash(node)}` —
/// for the same reason `deployment.rs`'s new ReplicaSet name is: two
/// overlapping reconciles (a relist after a 410, the routine watch-cache
/// lag between create and that Pod's own watch event landing) must
/// collide on one name and hit 409, not create a second Pod on the same
/// Node.
fn pod_name_for_node(ds_name: &str, node_name: &str) -> String {
    format!("{ds_name}-{}", compute_template_hash(&node_name))
}

fn owner_reference(ds: &DaemonSet) -> OwnerReference {
    OwnerReference {
        api_version: "apps/v1".to_string(),
        kind: "DaemonSet".to_string(),
        name: ds.name_any(),
        uid: ds.uid().unwrap_or_default(),
        controller: Some(true),
        block_owner_deletion: Some(true),
        ..Default::default()
    }
}

fn owned_by(pod: &Pod, ds_uid: &str) -> bool {
    pod.metadata
        .owner_references
        .as_ref()
        .into_iter()
        .flatten()
        .any(|o| o.controller == Some(true) && o.uid == ds_uid)
}

fn pod_ready(pod: &Pod) -> bool {
    pod.status
        .as_ref()
        .and_then(|s| s.conditions.as_ref())
        .into_iter()
        .flatten()
        .any(|c| c.type_ == "Ready" && c.status == "True")
}

fn pod_hash(pod: &Pod) -> Option<&str> {
    pod.metadata
        .labels
        .as_ref()
        .and_then(|l| l.get(POD_TEMPLATE_HASH_LABEL))
        .map(|s| s.as_str())
}

fn build_pod(ds: &DaemonSet, node_name: &str, hash: &str) -> Option<Pod> {
    let spec = ds.spec.as_ref()?;
    let mut pod_spec = spec.template.spec.clone()?;
    pod_spec.node_name = Some(node_name.to_string());
    let mut labels = spec
        .template
        .metadata
        .as_ref()
        .and_then(|m| m.labels.clone())
        .unwrap_or_default();
    labels.insert(POD_TEMPLATE_HASH_LABEL.to_string(), hash.to_string());
    let annotations = spec
        .template
        .metadata
        .as_ref()
        .and_then(|m| m.annotations.clone());
    Some(Pod {
        metadata: ObjectMeta {
            name: Some(pod_name_for_node(&ds.name_any(), node_name)),
            namespace: ds.namespace(),
            labels: Some(labels),
            annotations,
            owner_references: Some(vec![owner_reference(ds)]),
            ..Default::default()
        },
        spec: Some(pod_spec),
        ..Default::default()
    })
}

async fn reconcile_daemon_set(
    client: &Client,
    ds: &DaemonSet,
    node_cache: &HashMap<String, Node>,
    pod_cache: &HashMap<String, Pod>,
) {
    let namespace = ns_of(ds);
    let name = ds.name_any();
    let ds_api: Api<DaemonSet> = Api::namespaced(client.clone(), &namespace);
    let pod_api: Api<Pod> = Api::namespaced(client.clone(), &namespace);
    let Some(ds_uid) = ds.uid() else { return };
    let Some(spec) = ds.spec.as_ref() else { return };
    let node_selector = spec
        .template
        .spec
        .as_ref()
        .and_then(|s| s.node_selector.clone())
        .unwrap_or_default();
    let tolerations = spec
        .template
        .spec
        .as_ref()
        .and_then(|s| s.tolerations.clone())
        .unwrap_or_default();
    let hash = compute_template_hash(&spec.template);

    let eligible_nodes: Vec<&Node> = node_cache
        .values()
        .filter(|n| node_eligible(n, &node_selector, &tolerations))
        .collect();

    let owned: Vec<&Pod> = pod_cache
        .values()
        .filter(|p| p.namespace().as_deref() == Some(namespace.as_str()))
        .filter(|p| owned_by(p, &ds_uid))
        .collect();

    // Node -> its daemon Pod, if any (live, non-terminating).
    let mut by_node: HashMap<String, &Pod> = HashMap::new();
    for p in owned
        .iter()
        .filter(|p| p.metadata.deletion_timestamp.is_none())
    {
        if let Some(node_name) = p.spec.as_ref().and_then(|s| s.node_name.clone()) {
            by_node.insert(node_name, p);
        }
    }

    // Misscheduled: a live daemon Pod on a Node that's no longer eligible.
    let eligible_node_names: std::collections::HashSet<String> =
        eligible_nodes.iter().map(|n| n.name_any()).collect();
    let mut misscheduled = 0;
    for (node_name, pod) in &by_node {
        if !eligible_node_names.contains(node_name) {
            misscheduled += 1;
            if let Err(e) = pod_api.delete(&pod.name_any(), &Default::default()).await {
                tracing::warn!(namespace = %namespace, daemonset = %name, pod = %pod.name_any(), error = ?e, "failed to delete misscheduled DaemonSet Pod");
            }
        }
    }

    // Missing: an eligible Node with no daemon Pod at all — create one.
    for node in &eligible_nodes {
        let node_name = node.name_any();
        if by_node.contains_key(&node_name) {
            continue;
        }
        let Some(pod) = build_pod(&ds, &node_name, &hash) else {
            tracing::warn!(namespace = %namespace, daemonset = %name, "DaemonSet has no pod template — cannot create Pods");
            break;
        };
        match pod_api.create(&PostParams::default(), &pod).await {
            Ok(_) => {}
            Err(kube::Error::Api(ref status)) if status.is_already_exists() => {} // routine cache-lag race, see build_pod's doc
            Err(e) => {
                tracing::warn!(namespace = %namespace, daemonset = %name, node = %node_name, error = ?e, "failed to create DaemonSet Pod")
            }
        }
    }

    // Rolling update: replace outdated Pods on eligible Nodes, budget-limited.
    let rolling = spec
        .update_strategy
        .as_ref()
        .and_then(|s| s.rolling_update.as_ref());
    let desired = eligible_nodes.len() as i32;
    // Upstream's DaemonSet default is the absolute value 1, not a percent —
    // unlike Deployment's maxUnavailable/maxSurge, which both default to a
    // percentage (see deployment.rs's resolve_int_or_str default_percent).
    let max_unavailable = match rolling.and_then(|r| r.max_unavailable.as_ref()) {
        Some(v) => crate::controllers::deployment::resolve_int_or_str(Some(v), desired, false, 0),
        None => 1,
    };
    // Every eligible Node without a ready DaemonSet Pod is unavailable,
    // including a Pod created earlier in this reconcile. Ignore Pods on
    // ineligible Nodes because they are outside this rollout's desired set.
    let ready_on_eligible = by_node
        .iter()
        .filter(|(node, pod)| eligible_node_names.contains(*node) && pod_ready(pod))
        .count() as i32;
    let already_unavailable = (desired - ready_on_eligible).max(0);
    let mut budget = (max_unavailable - already_unavailable).max(0);
    let mut outdated: Vec<&&Pod> = by_node
        .iter()
        .filter(|(node_name, _)| eligible_node_names.contains(*node_name))
        .map(|(_, p)| p)
        .filter(|p| pod_hash(p) != Some(hash.as_str()))
        .collect();
    outdated.sort_by_key(|p| p.name_any());
    for pod in outdated {
        if budget <= 0 {
            break;
        }
        if let Err(e) = pod_api.delete(&pod.name_any(), &Default::default()).await {
            tracing::warn!(namespace = %namespace, daemonset = %name, pod = %pod.name_any(), error = ?e, "failed to delete outdated DaemonSet Pod for rolling update");
        }
        budget -= 1;
    }

    let current_scheduled = by_node
        .iter()
        .filter(|(n, _)| eligible_node_names.contains(*n))
        .count() as i32;
    let ready = by_node
        .iter()
        .filter(|(n, p)| eligible_node_names.contains(*n) && pod_ready(p))
        .count() as i32;
    let updated = by_node
        .iter()
        .filter(|(n, p)| eligible_node_names.contains(*n) && pod_hash(p) == Some(hash.as_str()))
        .count() as i32;
    let status = DaemonSetStatus {
        desired_number_scheduled: desired,
        current_number_scheduled: current_scheduled,
        number_ready: ready,
        number_available: Some(ready), // minReadySeconds not tracked, see module doc
        number_unavailable: Some((desired - ready).max(0)),
        number_misscheduled: misscheduled,
        updated_number_scheduled: Some(updated),
        observed_generation: ds.metadata.generation,
        ..Default::default()
    };
    if ds.status.as_ref() != Some(&status) {
        let patch = serde_json::json!({ "status": status });
        if let Err(e) = ds_api
            .patch_status(&name, &PatchParams::default(), &Patch::Merge(&patch))
            .await
        {
            tracing::warn!(namespace = %namespace, daemonset = %name, error = ?e, "failed to patch DaemonSet status");
        }
    }
}

fn ns_of<K: ResourceExt>(obj: &K) -> String {
    obj.namespace().unwrap_or_default()
}

pub async fn run(client: Client, _cfg: &crate::config::Config) -> Result<()> {
    let mut nodes: HashMap<String, Node> = HashMap::new();
    let mut pods: HashMap<String, Pod> = HashMap::new();
    let mut daemon_sets: HashMap<String, DaemonSet> = HashMap::new();
    let queue: KeyedWorkQueue<String> = KeyedWorkQueue::default();

    let mut node_stream = crate::watch::watch_nodes(&client);
    let mut pod_stream = crate::watch::watch_pods(&client);
    let mut ds_stream = crate::watch::watch_daemon_sets(&client);

    loop {
        tokio::select! {
            ev = node_stream.next() => {
                match ev {
                    Some(Ok(Event::Apply(n))) | Some(Ok(Event::InitApply(n))) => {
                        nodes.insert(n.name_any(), n);
                        for key in daemon_sets.keys() {
                            queue.enqueue(key.clone());
                        }
                    }
                    Some(Ok(Event::Delete(n))) => {
                        nodes.remove(&n.name_any());
                        for key in daemon_sets.keys() {
                            queue.enqueue(key.clone());
                        }
                    }
                    Some(Ok(Event::Init | Event::InitDone)) => {}
                    Some(Err(e)) => tracing::warn!(error = ?e, "node watch error in daemonset-controller"),
                    None => return Ok(()),
                }
            }
            ev = pod_stream.next() => {
                match ev {
                    Some(Ok(Event::Apply(pod))) | Some(Ok(Event::InitApply(pod))) => {
                        let ns = ns_of(&pod);
                        pods.insert(format!("{ns}/{}", pod.name_any()), pod);
                        for (key, ds) in daemon_sets.iter().filter(|(_, ds)| ns_of(ds) == ns) {
                            queue.enqueue(key.clone());
                        }
                    }
                    Some(Ok(Event::Delete(pod))) => {
                        let ns = ns_of(&pod);
                        pods.remove(&format!("{ns}/{}", pod.name_any()));
                        for (key, ds) in daemon_sets.iter().filter(|(_, ds)| ns_of(ds) == ns) {
                            queue.enqueue(key.clone());
                        }
                    }
                    Some(Ok(Event::Init | Event::InitDone)) => {}
                    Some(Err(e)) => tracing::warn!(error = ?e, "pod watch error in daemonset-controller"),
                    None => return Ok(()),
                }
            }
            ev = ds_stream.next() => {
                match ev {
                    Some(Ok(Event::Apply(ds))) | Some(Ok(Event::InitApply(ds))) => {
                        let ns = ns_of(&ds);
                        let name = ds.name_any();
                        let key = format!("{ns}/{name}");
                        daemon_sets.insert(key.clone(), ds);
                        queue.enqueue(key);
                    }
                    Some(Ok(Event::Delete(ds))) => {
                        daemon_sets.remove(&format!("{}/{}", ns_of(&ds), ds.name_any()));
                    }
                    Some(Ok(Event::Init | Event::InitDone)) => {}
                    Some(Err(e)) => tracing::warn!(error = ?e, "daemonset watch error in daemonset-controller"),
                    None => return Ok(()),
                }
            }
            key = queue.pop() => {
                if let Some(ds) = daemon_sets.get(&key).cloned() {
                    reconcile_daemon_set(&client, &ds, &nodes, &pods).await;
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
    fn empty_node_selector_matches_every_node() {
        assert!(node_selector_matches(
            &BTreeMap::new(),
            &labels(&[("zone", "a")])
        ));
    }

    #[test]
    fn node_selector_requires_every_pair() {
        let sel = labels(&[("zone", "a")]);
        assert!(node_selector_matches(
            &sel,
            &labels(&[("zone", "a"), ("gpu", "true")])
        ));
        assert!(!node_selector_matches(&sel, &labels(&[("zone", "b")])));
    }

    fn taint(key: &str, value: &str, effect: &str) -> Taint {
        Taint {
            key: key.to_string(),
            value: Some(value.to_string()),
            effect: effect.to_string(),
            time_added: None,
        }
    }

    fn toleration_exists(key: &str, effect: Option<&str>) -> Toleration {
        Toleration {
            key: Some(key.to_string()),
            operator: Some("Exists".to_string()),
            effect: effect.map(|e| e.to_string()),
            value: None,
            toleration_seconds: None,
        }
    }

    #[test]
    fn no_schedule_taint_requires_a_toleration() {
        let taints = vec![taint("dedicated", "gpu", "NoSchedule")];
        assert!(!taints_tolerated(&taints, &[]));
        assert!(taints_tolerated(
            &taints,
            &[toleration_exists("dedicated", None)]
        ));
    }

    #[test]
    fn prefer_no_schedule_taint_does_not_need_tolerating() {
        let taints = vec![taint("soft", "x", "PreferNoSchedule")];
        assert!(taints_tolerated(&taints, &[]));
    }

    #[test]
    fn a_toleration_scoped_to_the_wrong_effect_does_not_match() {
        let taints = vec![taint("dedicated", "gpu", "NoExecute")];
        assert!(!taints_tolerated(
            &taints,
            &[toleration_exists("dedicated", Some("NoSchedule"))]
        ));
    }

    #[test]
    fn pod_names_for_the_same_node_are_stable_and_distinct_across_nodes() {
        assert_eq!(
            pod_name_for_node("ds", "node-a"),
            pod_name_for_node("ds", "node-a")
        );
        assert_ne!(
            pod_name_for_node("ds", "node-a"),
            pod_name_for_node("ds", "node-b")
        );
    }
}
