//! resourcequota-controller (Group D): keeps `ResourceQuota.status.used`
//! up to date. Pure event — a namespace's object count changes exactly
//! when a watched object is created or deleted, nothing to poll.
//!
//! # What this controller does NOT do — and never can
//!
//! Upstream splits ResourceQuota into two entirely separate mechanisms,
//! and it's worth being explicit about which one this is: the
//! **`ResourceQuota` admission plugin**, which runs *inside kube-apiserver
//! itself* and synchronously rejects a create that would exceed
//! `spec.hard`, is not something a controller-manager replacement can
//! implement at all — it isn't part of kube-controller-manager's process,
//! it's apiserver's. `resourcequota-controller` (this file) only keeps
//! `status.used` current for observability (`kubectl describe quota`) —
//! actual enforcement already works today, unmodified, because it's the
//! real apiserver's job regardless of which controller-manager runs.
//!
//! # Scope of this slice
//!
//! Object-count quotas only (`pods`, `services`), and only those two
//! resource kinds — not the full upstream set (`configmaps`, `secrets`,
//! `persistentvolumeclaims`, `replicationcontrollers`, ...) and not
//! compute-resource quotas (`requests.cpu`, `limits.memory`, ...).
//! Compute-resource quotas need real `Quantity` arithmetic (binary/decimal
//! SI suffix parsing and summing) that
//! `k8s_openapi::apimachinery::pkg::api::resource::Quantity` doesn't
//! provide — `Quantity` is a bare string newtype with no arithmetic at
//! all. That's real, separate work, not a small extension of this file;
//! deferred rather than attempted half-correct. `pods`/`services` were
//! chosen as the two most commonly set object-count quotas in practice.
//! Every unsupported key in `spec.hard` is simply left absent from
//! `status.used` — never guessed at — so `kubectl describe quota` shows
//! exactly what this controller does and doesn't track, not a wrong number.

use anyhow::Result;
use futures::StreamExt;
use k8s_openapi::api::core::v1::{Pod, ResourceQuota, Service};
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use kube::api::{Api, Patch, PatchParams};
use kube::runtime::watcher::Event;
use kube::{Client, ResourceExt};
use std::collections::{BTreeMap, HashMap, HashSet};
use crate::workqueue::KeyedWorkQueue;

const SUPPORTED_KEYS: &[&str] = &["pods", "services"];

fn counts_toward_pod_quota(pod: &Pod) -> bool {
    !matches!(
        pod.status
            .as_ref()
            .and_then(|status| status.phase.as_deref()),
        Some("Succeeded") | Some("Failed")
    )
}

/// Pure: `status.used` for whichever of `hard_keys` this controller
/// actually tracks. Split out from the reconcile loop so the "which keys
/// get a number, which are left alone" rule is unit-testable without a
/// cluster — the same discipline every pure decision function in this
/// crate follows.
pub fn compute_used(
    hard_keys: &[String],
    pod_count: usize,
    service_count: usize,
) -> BTreeMap<String, Quantity> {
    let mut used = BTreeMap::new();
    for key in hard_keys {
        let count = match key.as_str() {
            "pods" => pod_count,
            "services" => service_count,
            _ => continue, // not one of SUPPORTED_KEYS — left untracked, not guessed
        };
        used.insert(key.clone(), Quantity(count.to_string()));
    }
    used
}

async fn reconcile_quota(
    client: &Client,
    quota: &ResourceQuota,
    pods: &HashMap<String, HashSet<String>>,
    services: &HashMap<String, HashSet<String>>,
) {
    let namespace = ns_of(quota);
    let name = quota.name_any();
    let api: Api<ResourceQuota> = Api::namespaced(client.clone(), &namespace);
    let hard = quota.spec.as_ref().and_then(|s| s.hard.as_ref());
    let Some(hard) = hard else { return };
    let hard_keys: Vec<String> = hard.keys().cloned().collect();

    let pod_count = pods.get(&namespace).map(|s| s.len()).unwrap_or(0);
    let service_count = services.get(&namespace).map(|s| s.len()).unwrap_or(0);
    let used = compute_used(&hard_keys, pod_count, service_count);

    if quota.status.as_ref().and_then(|s| s.used.as_ref()) == Some(&used) {
        return; // already correct
    }

    let patch = serde_json::json!({ "status": { "used": used, "hard": hard } });
    if let Err(e) = api
        .patch_status(&name, &PatchParams::default(), &Patch::Merge(&patch))
        .await
    {
        tracing::warn!(namespace = %namespace, quota = %name, error = ?e, "failed to patch ResourceQuota status");
    }
}

fn ns_of<K: ResourceExt>(obj: &K) -> String {
    obj.namespace().unwrap_or_default()
}

pub async fn run(client: Client, _cfg: &crate::config::Config) -> Result<()> {
    let mut pods: HashMap<String, HashSet<String>> = HashMap::new();
    let mut services: HashMap<String, HashSet<String>> = HashMap::new();
    let mut quotas: HashMap<String, ResourceQuota> = HashMap::new();
    let queue: KeyedWorkQueue<String> = KeyedWorkQueue::default();

    let mut pod_stream = crate::watch::watch_pods(&client);
    let mut svc_stream = crate::watch::watch_services(&client);
    let mut quota_stream = crate::watch::watch_resource_quotas(&client);

    loop {
        tokio::select! {
            ev = pod_stream.next() => {
                match ev {
                    Some(Ok(Event::Apply(p))) | Some(Ok(Event::InitApply(p))) => {
                        let ns = ns_of(&p);
                        if counts_toward_pod_quota(&p) {
                            pods.entry(ns.clone()).or_default().insert(p.name_any());
                        } else if let Some(set) = pods.get_mut(&ns) {
                            set.remove(&p.name_any());
                        }
                        enqueue_namespace_quotas(&ns, &quotas, &queue);
                    }
                    Some(Ok(Event::Delete(p))) => {
                        let ns = ns_of(&p);
                        if let Some(set) = pods.get_mut(&ns) { set.remove(&p.name_any()); }
                        enqueue_namespace_quotas(&ns, &quotas, &queue);
                    }
                    Some(Ok(Event::Init | Event::InitDone)) => {}
                    Some(Err(e)) => tracing::warn!(error = ?e, "pod watch error in resourcequota-controller"),
                    None => return Ok(()),
                }
            }
            ev = svc_stream.next() => {
                match ev {
                    Some(Ok(Event::Apply(s))) | Some(Ok(Event::InitApply(s))) => {
                        let ns = ns_of(&s);
                        services.entry(ns.clone()).or_default().insert(s.name_any());
                        enqueue_namespace_quotas(&ns, &quotas, &queue);
                    }
                    Some(Ok(Event::Delete(s))) => {
                        let ns = ns_of(&s);
                        if let Some(set) = services.get_mut(&ns) { set.remove(&s.name_any()); }
                        enqueue_namespace_quotas(&ns, &quotas, &queue);
                    }
                    Some(Ok(Event::Init | Event::InitDone)) => {}
                    Some(Err(e)) => tracing::warn!(error = ?e, "service watch error in resourcequota-controller"),
                    None => return Ok(()),
                }
            }
            ev = quota_stream.next() => {
                match ev {
                    Some(Ok(Event::Apply(q))) | Some(Ok(Event::InitApply(q))) => {
                        let ns = ns_of(&q);
                        let name = q.name_any();
                        quotas.insert(format!("{ns}/{name}"), q);
                        queue.enqueue(format!("{ns}/{name}"));
                    }
                    Some(Ok(Event::Delete(q))) => {
                        quotas.remove(&format!("{}/{}", ns_of(&q), q.name_any()));
                    }
                    Some(Ok(Event::Init | Event::InitDone)) => {}
                    Some(Err(e)) => tracing::warn!(error = ?e, "resourcequota watch error in resourcequota-controller"),
                    None => return Ok(()),
                }
            }
            key = queue.pop() => {
                if let Some(quota) = quotas.get(&key).cloned() {
                    reconcile_quota(&client, &quota, &pods, &services).await;
                }
            }
        }
    }
}

fn enqueue_namespace_quotas(
    namespace: &str,
    quotas: &HashMap<String, ResourceQuota>,
    queue: &KeyedWorkQueue<String>,
) {
    for (key, quota) in quotas {
        if ns_of(quota) == namespace {
            queue.enqueue(key.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tracks_pods_and_services_when_both_are_requested() {
        let hard = vec!["pods".to_string(), "services".to_string()];
        let used = compute_used(&hard, 3, 1);
        assert_eq!(used.get("pods"), Some(&Quantity("3".to_string())));
        assert_eq!(used.get("services"), Some(&Quantity("1".to_string())));
    }

    #[test]
    fn an_unsupported_key_is_left_out_entirely_not_guessed() {
        let hard = vec!["requests.cpu".to_string(), "pods".to_string()];
        let used = compute_used(&hard, 2, 0);
        assert!(!used.contains_key("requests.cpu"));
        assert_eq!(used.get("pods"), Some(&Quantity("2".to_string())));
    }

    #[test]
    fn no_hard_keys_means_no_used_entries() {
        assert!(compute_used(&[], 5, 5).is_empty());
    }

    #[test]
    fn zero_objects_reports_zero_not_absence() {
        let hard = vec!["pods".to_string()];
        let used = compute_used(&hard, 0, 0);
        assert_eq!(used.get("pods"), Some(&Quantity("0".to_string())));
    }

    #[test]
    fn supported_keys_list_matches_what_compute_used_actually_handles() {
        // Guards against the two ever drifting apart — SUPPORTED_KEYS is
        // documentation-facing (the module doc references it), compute_used
        // is the real behavior.
        for key in SUPPORTED_KEYS {
            let used = compute_used(&[key.to_string()], 1, 1);
            assert!(
                used.contains_key(*key),
                "{key} is listed as supported but compute_used doesn't handle it"
            );
        }
    }
}
