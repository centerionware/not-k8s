//! disruption-controller (Group J): keeps a `PodDisruptionBudget`'s
//! `.status` (`currentHealthy`, `desiredHealthy`, `disruptionsAllowed`,
//! `expectedPods`) current. This controller never blocks an eviction
//! itself — that's the apiserver's own `policy/v1` eviction subresource
//! admission, which reads `status.disruptionsAllowed` and rejects a
//! request that would take it negative. Without this controller that
//! field never updates, so eviction callers (`kubectl drain`,
//! cluster-autoscaler-style tooling) see a permanently stale value —
//! either wrongly blocked forever (if it starts at `0`) or wrongly
//! unblocked forever (if it starts positive and pods later become
//! unhealthy).
//!
//! # Scope of this slice
//!
//! **A pod counts as "healthy" via `Ready=True`, and only that** — matches
//! `PodDisruptionBudgetSpec.unhealthyPodEvictionPolicy`'s own doc comment
//! ("healthy pods, as pods with a Ready=True condition"), but this
//! controller does not implement the `AlwaysAllow`/`IfHealthyBudget`
//! `unhealthyPodEvictionPolicy` distinction itself — that policy only
//! matters for the apiserver's own eviction-admission decision, not for
//! computing status, so it's out of this controller's scope entirely, not
//! a gap.
//!
//! **No `status.conditions`/`DisruptionAllowed` condition, no
//! `status.disruptedPods` bookkeeping** — both are upstream niceties (a
//! human-readable condition mirroring `disruptionsAllowed`, and a
//! short-lived map tracking evictions in flight) that nothing in this
//! project reads; the numeric status fields alone are what the apiserver's
//! own admission check and `kubectl` actually consume.
//!
//! **A PDB with neither `minAvailable` nor `maxUnavailable` set** (invalid
//! per upstream's own admission validation, which requires exactly one,
//! but not re-validated here) **is treated as "no restriction"** —
//! `desiredHealthy` is `0`, every disruption is allowed. Fails open, not
//! closed, on a malformed spec this controller shouldn't have been asked
//! to interpret in the first place.

use anyhow::{Context, Result};
use futures::StreamExt;
use k8s_openapi::api::core::v1::Pod;
use k8s_openapi::api::policy::v1::{PodDisruptionBudget, PodDisruptionBudgetStatus};
use kube::api::{Api, Patch, PatchParams};
use kube::runtime::watcher::Event;
use kube::{Client, ResourceExt};
use std::collections::HashMap;

fn pod_ready(pod: &Pod) -> bool {
    pod.status.as_ref().and_then(|s| s.conditions.as_ref()).into_iter().flatten().any(|c| c.type_ == "Ready" && c.status == "True")
}

/// The four numeric status fields — pure given `expected`/`healthy` counts
/// and the spec's own `minAvailable`/`maxUnavailable`, so the budget
/// arithmetic is unit-testable without any live Pod/PDB objects.
pub fn compute_status(
    expected: i32,
    healthy: i32,
    min_available: Option<&k8s_openapi::apimachinery::pkg::util::intstr::IntOrString>,
    max_unavailable: Option<&k8s_openapi::apimachinery::pkg::util::intstr::IntOrString>,
) -> (i32, i32, i32) {
    let desired_healthy = if let Some(min) = min_available {
        crate::controllers::deployment::resolve_int_or_str(Some(min), expected, true, 100)
    } else if let Some(max) = max_unavailable {
        (expected - crate::controllers::deployment::resolve_int_or_str(Some(max), expected, false, 0)).max(0)
    } else {
        0
    };
    let disruptions_allowed = (healthy - desired_healthy).max(0);
    (desired_healthy, disruptions_allowed, expected)
}

async fn reconcile_pdb(client: &Client, namespace: &str, name: &str, pods: &HashMap<String, Pod>) {
    let api: Api<PodDisruptionBudget> = Api::namespaced(client.clone(), namespace);
    let pdb = match api.get_opt(name).await {
        Ok(Some(p)) => p,
        Ok(None) => return,
        Err(e) => {
            tracing::warn!(namespace = %namespace, pdb = %name, error = ?e, "failed to read PodDisruptionBudget for reconcile");
            return;
        }
    };
    let Some(selector) = pdb.spec.as_ref().and_then(|s| s.selector.as_ref()) else { return };
    let spec = pdb.spec.clone().unwrap_or_default();

    let matching: Vec<&Pod> = pods
        .values()
        .filter(|p| p.namespace().as_deref() == Some(namespace))
        .filter(|p| p.metadata.deletion_timestamp.is_none())
        .filter(|p| p.metadata.labels.as_ref().is_some_and(|l| crate::controllers::replica_set::label_selector_matches(selector, l)))
        .collect();
    let expected = matching.len() as i32;
    let healthy = matching.iter().filter(|p| pod_ready(p)).count() as i32;
    let (desired_healthy, disruptions_allowed, expected_pods) =
        compute_status(expected, healthy, spec.min_available.as_ref(), spec.max_unavailable.as_ref());

    let status = PodDisruptionBudgetStatus {
        current_healthy: healthy,
        desired_healthy,
        disruptions_allowed,
        expected_pods,
        observed_generation: pdb.metadata.generation,
        ..pdb.status.clone().unwrap_or_default()
    };
    if pdb.status.as_ref() == Some(&status) {
        return;
    }
    let patch = serde_json::json!({ "status": status });
    if let Err(e) = api.patch_status(name, &PatchParams::default(), &Patch::Merge(&patch)).await {
        tracing::warn!(namespace = %namespace, pdb = %name, error = ?e, "failed to patch PodDisruptionBudget status");
    }
}

fn ns_of<K: ResourceExt>(obj: &K) -> String {
    obj.namespace().unwrap_or_default()
}

pub async fn run(client: Client, _cfg: &crate::config::Config) -> Result<()> {
    let mut pods: HashMap<String, Pod> = HashMap::new();
    let mut pdbs: std::collections::HashSet<(String, String)> = std::collections::HashSet::new();

    let pod_api: Api<Pod> = Api::all(client.clone());
    let pdb_api: Api<PodDisruptionBudget> = Api::all(client.clone());

    for p in pod_api.list(&Default::default()).await.context("listing Pods to seed disruption-controller")?.items {
        pods.insert(format!("{}/{}", ns_of(&p), p.name_any()), p);
    }
    for pdb in pdb_api.list(&Default::default()).await.context("listing PodDisruptionBudgets to seed disruption-controller")?.items {
        let ns = ns_of(&pdb);
        let name = pdb.name_any();
        pdbs.insert((ns.clone(), name.clone()));
        reconcile_pdb(&client, &ns, &name, &pods).await;
    }

    let mut pod_stream = crate::watch::watch_pods(&client);
    let mut pdb_stream = crate::watch::watch_pod_disruption_budgets(&client);

    loop {
        tokio::select! {
            ev = pod_stream.next() => {
                match ev {
                    Some(Ok(Event::Apply(pod))) | Some(Ok(Event::InitApply(pod))) => {
                        let ns = ns_of(&pod);
                        pods.insert(format!("{ns}/{}", pod.name_any()), pod);
                        for (pdb_ns, pdb_name) in pdbs.iter().filter(|(n, _)| *n == ns) {
                            reconcile_pdb(&client, pdb_ns, pdb_name, &pods).await;
                        }
                    }
                    Some(Ok(Event::Delete(pod))) => {
                        let ns = ns_of(&pod);
                        pods.remove(&format!("{ns}/{}", pod.name_any()));
                        for (pdb_ns, pdb_name) in pdbs.iter().filter(|(n, _)| *n == ns) {
                            reconcile_pdb(&client, pdb_ns, pdb_name, &pods).await;
                        }
                    }
                    Some(Ok(Event::Init | Event::InitDone)) => {}
                    Some(Err(e)) => tracing::warn!(error = ?e, "pod watch error in disruption-controller"),
                    None => return Ok(()),
                }
            }
            ev = pdb_stream.next() => {
                match ev {
                    Some(Ok(Event::Apply(pdb))) | Some(Ok(Event::InitApply(pdb))) => {
                        let ns = ns_of(&pdb);
                        let name = pdb.name_any();
                        pdbs.insert((ns.clone(), name.clone()));
                        reconcile_pdb(&client, &ns, &name, &pods).await;
                    }
                    Some(Ok(Event::Delete(pdb))) => { pdbs.remove(&(ns_of(&pdb), pdb.name_any())); }
                    Some(Ok(Event::Init | Event::InitDone)) => {}
                    Some(Err(e)) => tracing::warn!(error = ?e, "pdb watch error in disruption-controller"),
                    None => return Ok(()),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use k8s_openapi::apimachinery::pkg::util::intstr::IntOrString;

    #[test]
    fn min_available_absolute() {
        let min = IntOrString::Int(2);
        let (desired, allowed, expected) = compute_status(5, 4, Some(&min), None);
        assert_eq!((desired, allowed, expected), (2, 2, 5));
    }

    #[test]
    fn max_unavailable_absolute() {
        let max = IntOrString::Int(1);
        let (desired, allowed, _) = compute_status(5, 5, None, Some(&max));
        assert_eq!((desired, allowed), (4, 1));
    }

    #[test]
    fn disruptions_allowed_never_goes_negative() {
        let min = IntOrString::Int(4);
        let (_, allowed, _) = compute_status(5, 2, Some(&min), None);
        assert_eq!(allowed, 0);
    }

    #[test]
    fn neither_field_set_allows_everything() {
        let (desired, allowed, _) = compute_status(5, 1, None, None);
        assert_eq!((desired, allowed), (0, 1));
    }

    #[test]
    fn min_available_percent_rounds_up() {
        let min = IntOrString::String("50%".to_string());
        // 3 total, 50% -> 1.5 -> rounds up to 2 (minAvailable rounds up).
        let (desired, _, _) = compute_status(3, 3, Some(&min), None);
        assert_eq!(desired, 2);
    }
}
