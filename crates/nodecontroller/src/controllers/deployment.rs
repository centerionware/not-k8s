//! deployment-controller (Group E, workload controllers): manages
//! ReplicaSets, not Pods directly — every Deployment owns one ReplicaSet
//! per distinct Pod template ("revision"), and this controller's job is
//! creating the next one, then shifting replica counts from the old one(s)
//! to the new one according to `spec.strategy` (rolling update surge/
//! unavailable budgets, or Recreate). `replicaset_controller.rs` (which
//! this file has a hard, one-directional dependency on — nothing here
//! creates a Pod) is what turns each ReplicaSet's `spec.replicas` into
//! real Pods.
//!
//! # Scope of this slice
//!
//! **Revision identity**: upstream computes a Pod-template hash
//! (`pod-template-hash` label) with a specific FNV-based algorithm so a
//! rollback/kubectl-diff can recognize a previously-seen revision by its
//! hash across restarts of the controller itself. This file computes its
//! *own* hash (FNV-1a over the template's canonical JSON) that is
//! internally consistent (same template ⇒ same hash, always, including
//! across a controller restart, since it's a pure function of the
//! template) but numerically different from upstream's. Nothing in this
//! crate or `kubectl` compares the two, so this is a real but harmless
//! difference — worth naming rather than silently diverging.
//!
//! **No collision-count / hash-collision retry** — upstream appends
//! `status.collisionCount` into the hash input and bumps it on a name
//! clash. A clash here (two different templates hashing to the same
//! value) is astronomically unlikely and unhandled: the create simply
//! fails and is retried next reconcile, same object either way.
//!
//! **Old-ReplicaSet scale-down ranking is simplified**: oldest-created
//! first, budget-limited. Upstream ranks by "most unhealthy first, then
//! oldest" so a broken rollout sheds its own broken Pods before touching
//! healthy old ones; this file always drains the oldest ReplicaSet first
//! regardless of its health. A real difference in *which* old Pods go
//! first during a rollout, never in whether the rollout as a whole
//! converges or in the surge/unavailable budgets themselves.
//!
//! **`Recreate` strategy is coarse**: old ReplicaSets are scaled to 0 and
//! the new one is only scaled up once `status.replicas` (not just
//! `spec.replicas`) across all old ReplicaSets reads 0 — i.e. once the
//! ReplicaSet objects themselves report no Pods left, not once every Pod
//! has actually finished terminating on the node. Close enough in
//! practice (ReplicaSet status lags real Pod state by at most one
//! replicaset-controller reconcile), but not upstream's Pod-level wait.
//!
//! **No rollback (`kubectl rollout undo`), no revision history annotations,
//! no `DeploymentCondition`/`Progressing`/`ProgressDeadlineExceeded`
//! tracking** — `status.replicas`/`updatedReplicas`/`readyReplicas`/
//! `availableReplicas` are kept current; the human-readable condition list
//! is not populated. `spec.revisionHistoryLimit` *is* honored (old,
//! fully-scaled-down ReplicaSets beyond the limit are deleted), since that's
//! cheap and load-bearing for the common "don't accumulate ReplicaSets
//! forever" complaint.
//!
//! **Same owner-reference GC gap every controller in this crate has before
//! `garbage-collector-controller` exists**: deleting a Deployment does not
//! cascade-delete its ReplicaSets (though this controller *does* delete its
//! own old, empty ReplicaSets past `revisionHistoryLimit` — that's this
//! controller cleaning up after itself, not GC).

use anyhow::{Context, Result};
use futures::StreamExt;
use k8s_openapi::api::apps::v1::{Deployment, DeploymentStatus, ReplicaSet, ReplicaSetSpec};
use k8s_openapi::api::core::v1::PodTemplateSpec;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{LabelSelector, ObjectMeta, OwnerReference};
use k8s_openapi::apimachinery::pkg::util::intstr::IntOrString;
use kube::api::{Api, Patch, PatchParams, PostParams};
use kube::runtime::watcher::Event;
use kube::{Client, ResourceExt};
use std::collections::HashMap;

pub const POD_TEMPLATE_HASH_LABEL: &str = "pod-template-hash";

/// FNV-1a over the template's canonical JSON — see module doc for why this
/// deliberately isn't upstream's own hash algorithm.
fn compute_template_hash(template: &PodTemplateSpec) -> String {
    let bytes = serde_json::to_vec(template).unwrap_or_default();
    let mut hash: u64 = 0xcbf29ce484222325;
    for b in bytes {
        hash ^= b as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{hash:x}")
}

/// Resolve a `maxSurge`/`maxUnavailable`-shaped `IntOrString` against
/// `total`: a bare int is absolute, a `"NN%"` string is a percentage of
/// `total` rounded per `round_up` (upstream: surge rounds up, unavailable
/// rounds down), `None` uses `default_percent`.
pub fn resolve_int_or_str(v: Option<&IntOrString>, total: i32, round_up: bool, default_percent: i32) -> i32 {
    let (is_percent, num) = match v {
        Some(IntOrString::Int(n)) => (false, *n),
        Some(IntOrString::String(s)) => match s.strip_suffix('%') {
            Some(pct) => (true, pct.parse::<i32>().unwrap_or(default_percent)),
            None => (false, s.parse::<i32>().unwrap_or(0)),
        },
        None => (true, default_percent),
    };
    let resolved =
        if is_percent { if round_up { (total * num + 99) / 100 } else { (total * num) / 100 } } else { num };
    resolved.max(0)
}

/// `maxSurge` and `maxUnavailable` can't both resolve to 0 when there's
/// anything to roll — nothing could ever progress. Upstream's fencepost
/// fix: bump `maxSurge` to 1.
pub fn resolve_fenceposts(max_surge: i32, max_unavailable: i32, desired: i32) -> (i32, i32) {
    if max_surge == 0 && max_unavailable == 0 && desired > 0 { (1, max_unavailable) } else { (max_surge, max_unavailable) }
}

/// How many replicas the new ReplicaSet should have right now, given the
/// total Pod count across every ReplicaSet the Deployment owns (old + new)
/// and the surge budget.
pub fn new_rs_desired_replicas(desired: i32, max_surge: i32, current_total: i32, new_rs_current: i32) -> i32 {
    let max_total = desired + max_surge;
    if current_total >= max_total {
        return new_rs_current;
    }
    let scale_up = (max_total - current_total).min(desired - new_rs_current).max(0);
    new_rs_current + scale_up
}

/// How many total Pods (across every old ReplicaSet combined) may be
/// scaled down right now without breaching the unavailable budget.
pub fn scale_down_budget(desired: i32, max_unavailable: i32, available_total: i32, new_rs_replicas: i32, new_rs_available: i32) -> i32 {
    let min_available = desired - max_unavailable;
    let new_rs_unavailable = (new_rs_replicas - new_rs_available).max(0);
    (available_total - min_available - new_rs_unavailable).max(0)
}

/// Spends `budget` scaling down `old` (name, current_replicas), assumed
/// already ordered oldest-first by the caller — see module doc on the
/// scale-down ranking simplification. Returns only the ReplicaSets whose
/// count actually changed.
pub fn scale_down_old(old: Vec<(String, i32)>, budget: i32) -> Vec<(String, i32)> {
    let mut remaining = budget;
    let mut result = Vec::new();
    for (name, replicas) in old {
        if remaining <= 0 || replicas == 0 {
            continue;
        }
        let cut = replicas.min(remaining);
        remaining -= cut;
        result.push((name, replicas - cut));
    }
    result
}

fn is_recreate(deployment: &Deployment) -> bool {
    deployment.spec.as_ref().and_then(|s| s.strategy.as_ref()).and_then(|s| s.type_.as_deref()) == Some("Recreate")
}

fn owner_reference(d: &Deployment) -> OwnerReference {
    OwnerReference {
        api_version: "apps/v1".to_string(),
        kind: "Deployment".to_string(),
        name: d.name_any(),
        uid: d.uid().unwrap_or_default(),
        controller: Some(true),
        block_owner_deletion: Some(true),
        ..Default::default()
    }
}

fn owned_by(rs: &ReplicaSet, deployment_uid: &str) -> bool {
    rs.metadata.owner_references.as_ref().into_iter().flatten().any(|o| o.controller == Some(true) && o.uid == deployment_uid)
}

fn ns_of<K: ResourceExt>(obj: &K) -> String {
    obj.namespace().unwrap_or_default()
}

fn rs_selector_with_hash(base: &LabelSelector, hash: &str) -> LabelSelector {
    let mut match_labels = base.match_labels.clone().unwrap_or_default();
    match_labels.insert(POD_TEMPLATE_HASH_LABEL.to_string(), hash.to_string());
    LabelSelector { match_labels: Some(match_labels), match_expressions: base.match_expressions.clone() }
}

fn build_new_replica_set(d: &Deployment, hash: &str, replicas: i32) -> Option<ReplicaSet> {
    let spec = d.spec.as_ref()?;
    let mut template = spec.template.clone();
    let mut labels = template.metadata.as_ref().and_then(|m| m.labels.clone()).unwrap_or_default();
    labels.insert(POD_TEMPLATE_HASH_LABEL.to_string(), hash.to_string());
    let mut meta = template.metadata.clone().unwrap_or_default();
    meta.labels = Some(labels.clone());
    template.metadata = Some(meta);

    let rs_spec = ReplicaSetSpec {
        replicas: Some(replicas),
        selector: rs_selector_with_hash(&spec.selector, hash),
        template: Some(template),
        min_ready_seconds: spec.min_ready_seconds,
    };
    Some(ReplicaSet {
        metadata: ObjectMeta {
            // Deterministic, not `generateName`: the create race below
            // (two overlapping reconciles both finding no cached RS for
            // this hash yet, e.g. right after the watch cache hasn't
            // caught up) must collide on the *same* name and hit 409, not
            // silently create two independent ReplicaSets for one
            // revision — that's exactly the runaway-Pod bug this comment
            // replaced (confirmed live in CI: dozens of Pods across two
            // `generateName`d ReplicaSets for the same template).
            name: Some(format!("{}-{}", d.name_any(), hash)),
            namespace: d.namespace(),
            labels: Some(labels),
            owner_references: Some(vec![owner_reference(d)]),
            ..Default::default()
        },
        spec: Some(rs_spec),
        ..Default::default()
    })
}

fn rs_replicas(rs: &ReplicaSet) -> i32 {
    rs.spec.as_ref().and_then(|s| s.replicas).unwrap_or(0)
}

fn rs_available(rs: &ReplicaSet) -> i32 {
    rs.status.as_ref().and_then(|s| s.available_replicas).unwrap_or(0)
}

fn rs_ready(rs: &ReplicaSet) -> i32 {
    rs.status.as_ref().and_then(|s| s.ready_replicas).unwrap_or(0)
}

fn rs_hash(rs: &ReplicaSet) -> Option<&str> {
    rs.metadata.labels.as_ref().and_then(|l| l.get(POD_TEMPLATE_HASH_LABEL)).map(|s| s.as_str())
}

async fn scale_replica_set(rs_api: &Api<ReplicaSet>, name: &str, replicas: i32) {
    let patch = serde_json::json!({ "spec": { "replicas": replicas } });
    if let Err(e) = rs_api.patch(name, &PatchParams::default(), &Patch::Merge(&patch)).await {
        tracing::warn!(replicaset = %name, replicas, error = ?e, "failed to scale ReplicaSet for deployment-controller");
    }
}

async fn reconcile_deployment(client: &Client, namespace: &str, name: &str, rs_cache: &HashMap<String, ReplicaSet>) {
    let d_api: Api<Deployment> = Api::namespaced(client.clone(), namespace);
    let rs_api: Api<ReplicaSet> = Api::namespaced(client.clone(), namespace);

    let d = match d_api.get_opt(name).await {
        Ok(Some(d)) => d,
        Ok(None) => return, // gone — its ReplicaSets are Group D's job, see module doc
        Err(e) => {
            tracing::warn!(namespace = %namespace, deployment = %name, error = ?e, "failed to read Deployment for reconcile");
            return;
        }
    };
    let Some(d_uid) = d.uid() else { return };
    let Some(spec) = d.spec.as_ref() else { return };
    if spec.paused == Some(true) {
        return;
    }
    let desired = spec.replicas.unwrap_or(1);
    let hash = compute_template_hash(&spec.template);

    let mut owned: Vec<&ReplicaSet> = rs_cache
        .values()
        .filter(|rs| rs.namespace().as_deref() == Some(namespace))
        .filter(|rs| owned_by(rs, &d_uid))
        .collect();
    owned.sort_by_key(|rs| rs.metadata.creation_timestamp.clone().map(|t| t.0));

    let new_rs = owned.iter().find(|rs| rs_hash(rs) == Some(hash.as_str())).copied();

    // No matching-hash ReplicaSet exists yet: create it at 0 (RollingUpdate)
    // or `desired` (Recreate, once old ones are drained — checked below).
    let new_rs_owned;
    let new_rs: &ReplicaSet = match new_rs {
        Some(rs) => rs,
        None => {
            // Always created at 0 — RollingUpdate scales it up incrementally
            // below; Recreate only scales it up once old ReplicaSets are
            // drained, handled in the `is_recreate` branch further down.
            let Some(new) = build_new_replica_set(&d, &hash, 0) else { return };
            match rs_api.create(&PostParams::default(), &new).await {
                Ok(created) => {
                    new_rs_owned = created;
                    &new_rs_owned
                }
                // A concurrent reconcile (this one racing itself, or the
                // watch cache not having caught up yet) already created
                // the same deterministically-named RS — fetch the real
                // object rather than erroring the whole reconcile away.
                Err(kube::Error::Api(ref status)) if status.is_already_exists() => {
                    match rs_api.get(&new.name_any()).await {
                        Ok(existing) => {
                            new_rs_owned = existing;
                            &new_rs_owned
                        }
                        Err(e) => {
                            tracing::warn!(namespace = %namespace, deployment = %name, error = ?e, "failed to fetch already-existing new ReplicaSet for deployment-controller");
                            return;
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!(namespace = %namespace, deployment = %name, error = ?e, "failed to create new ReplicaSet for deployment-controller");
                    return;
                }
            }
        }
    };

    let old: Vec<&ReplicaSet> = owned.iter().filter(|rs| rs.name_any() != new_rs.name_any()).copied().collect();
    let old_total: i32 = old.iter().map(|rs| rs_replicas(rs)).sum();
    let old_available: i32 = old.iter().map(|rs| rs_available(rs)).sum();

    if is_recreate(&d) {
        // Scale every old RS to 0 first; only scale the new one up once
        // none of them report any Pods left (see module doc: RS-status
        // granularity, not real per-Pod wait).
        for rs in &old {
            if rs_replicas(rs) != 0 {
                scale_replica_set(&rs_api, &rs.name_any(), 0).await;
            }
        }
        let old_settled = old.iter().all(|rs| rs.status.as_ref().map(|s| s.replicas).unwrap_or(0) == 0);
        let target = if old_settled { desired } else { 0 };
        if rs_replicas(new_rs) != target {
            scale_replica_set(&rs_api, &new_rs.name_any(), target).await;
        }
    } else {
        let rolling = spec.strategy.as_ref().and_then(|s| s.rolling_update.as_ref());
        let max_surge = resolve_int_or_str(rolling.and_then(|r| r.max_surge.as_ref()), desired, true, 25);
        let max_unavailable = resolve_int_or_str(rolling.and_then(|r| r.max_unavailable.as_ref()), desired, false, 25);
        let (max_surge, max_unavailable) = resolve_fenceposts(max_surge, max_unavailable, desired);

        let current_total = old_total + rs_replicas(new_rs);
        let new_target = new_rs_desired_replicas(desired, max_surge, current_total, rs_replicas(new_rs));
        if new_target != rs_replicas(new_rs) {
            scale_replica_set(&rs_api, &new_rs.name_any(), new_target).await;
        }

        let available_total = old_available + rs_available(new_rs);
        let budget = scale_down_budget(desired, max_unavailable, available_total, new_target, rs_available(new_rs));
        let old_sorted: Vec<(String, i32)> = old.iter().map(|rs| (rs.name_any(), rs_replicas(rs))).collect();
        for (rs_name, new_count) in scale_down_old(old_sorted, budget) {
            scale_replica_set(&rs_api, &rs_name, new_count).await;
        }
    }

    // Clean up fully-drained old ReplicaSets past revisionHistoryLimit —
    // this controller's own cleanup of its own objects, not GC (see module
    // doc: no owner-reference cascade delete here yet).
    let limit = spec.revision_history_limit.unwrap_or(10).max(0) as usize;
    let mut drained: Vec<&&ReplicaSet> =
        old.iter().filter(|rs| rs_replicas(rs) == 0 && rs.status.as_ref().map(|s| s.replicas).unwrap_or(0) == 0).collect();
    drained.sort_by_key(|rs| rs.metadata.creation_timestamp.clone().map(|t| t.0));
    if drained.len() > limit {
        for rs in &drained[..drained.len() - limit] {
            if let Err(e) = rs_api.delete(&rs.name_any(), &Default::default()).await {
                tracing::warn!(namespace = %namespace, replicaset = %rs.name_any(), error = ?e, "failed to delete old ReplicaSet past revisionHistoryLimit");
            }
        }
    }

    let total_replicas: i32 = owned.iter().map(|rs| rs.status.as_ref().map(|s| s.replicas).unwrap_or(0)).sum();
    let updated_replicas = new_rs.status.as_ref().map(|s| s.replicas).unwrap_or(0);
    let ready_replicas: i32 = owned.iter().map(|rs| rs_ready(rs)).sum();
    let available_replicas: i32 = owned.iter().map(|rs| rs_available(rs)).sum();
    let status = DeploymentStatus {
        replicas: Some(total_replicas),
        updated_replicas: Some(updated_replicas),
        ready_replicas: Some(ready_replicas),
        available_replicas: Some(available_replicas),
        unavailable_replicas: Some((desired - available_replicas).max(0)),
        observed_generation: d.metadata.generation,
        ..Default::default()
    };
    if d.status.as_ref() != Some(&status) {
        let patch = serde_json::json!({ "status": status });
        if let Err(e) = d_api.patch_status(name, &PatchParams::default(), &Patch::Merge(&patch)).await {
            tracing::warn!(namespace = %namespace, deployment = %name, error = ?e, "failed to patch Deployment status");
        }
    }
}

pub async fn run(client: Client, _cfg: &crate::config::Config) -> Result<()> {
    let mut replica_sets: HashMap<String, ReplicaSet> = HashMap::new();
    let mut deployments: std::collections::HashSet<(String, String)> = std::collections::HashSet::new();

    let rs_api: Api<ReplicaSet> = Api::all(client.clone());
    let d_api: Api<Deployment> = Api::all(client.clone());

    for rs in rs_api.list(&Default::default()).await.context("listing ReplicaSets to seed deployment-controller")?.items {
        replica_sets.insert(format!("{}/{}", ns_of(&rs), rs.name_any()), rs);
    }
    for d in d_api.list(&Default::default()).await.context("listing Deployments to seed deployment-controller")?.items {
        let ns = ns_of(&d);
        let name = d.name_any();
        deployments.insert((ns.clone(), name.clone()));
        reconcile_deployment(&client, &ns, &name, &replica_sets).await;
    }

    let mut rs_stream = crate::watch::watch_replica_sets(&client);
    let mut d_stream = crate::watch::watch_deployments(&client);

    loop {
        tokio::select! {
            ev = rs_stream.next() => {
                match ev {
                    Some(Ok(Event::Apply(rs))) | Some(Ok(Event::InitApply(rs))) => {
                        let ns = ns_of(&rs);
                        replica_sets.insert(format!("{ns}/{}", rs.name_any()), rs);
                        for (d_ns, d_name) in deployments.iter().filter(|(n, _)| *n == ns) {
                            reconcile_deployment(&client, d_ns, d_name, &replica_sets).await;
                        }
                    }
                    Some(Ok(Event::Delete(rs))) => {
                        let ns = ns_of(&rs);
                        replica_sets.remove(&format!("{ns}/{}", rs.name_any()));
                        for (d_ns, d_name) in deployments.iter().filter(|(n, _)| *n == ns) {
                            reconcile_deployment(&client, d_ns, d_name, &replica_sets).await;
                        }
                    }
                    Some(Ok(Event::Init | Event::InitDone)) => {}
                    Some(Err(e)) => tracing::warn!(error = ?e, "replicaset watch error in deployment-controller"),
                    None => return Ok(()),
                }
            }
            ev = d_stream.next() => {
                match ev {
                    Some(Ok(Event::Apply(d))) | Some(Ok(Event::InitApply(d))) => {
                        let ns = ns_of(&d);
                        let name = d.name_any();
                        deployments.insert((ns.clone(), name.clone()));
                        reconcile_deployment(&client, &ns, &name, &replica_sets).await;
                    }
                    Some(Ok(Event::Delete(d))) => {
                        deployments.remove(&(ns_of(&d), d.name_any()));
                    }
                    Some(Ok(Event::Init | Event::InitDone)) => {}
                    Some(Err(e)) => tracing::warn!(error = ?e, "deployment watch error in deployment-controller"),
                    None => return Ok(()),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn resolves_absolute_int() {
        assert_eq!(resolve_int_or_str(Some(&IntOrString::Int(3)), 10, true, 25), 3);
    }

    #[test]
    fn resolves_percent_rounding_up_for_surge() {
        // 25% of 10 == 2.5, surge rounds up
        assert_eq!(resolve_int_or_str(Some(&IntOrString::String("25%".to_string())), 10, true, 25), 3);
    }

    #[test]
    fn resolves_percent_rounding_down_for_unavailable() {
        assert_eq!(resolve_int_or_str(Some(&IntOrString::String("25%".to_string())), 10, false, 25), 2);
    }

    #[test]
    fn none_uses_default_percent() {
        assert_eq!(resolve_int_or_str(None, 4, true, 25), 1);
    }

    #[test]
    fn fenceposts_bump_surge_when_both_would_be_zero() {
        assert_eq!(resolve_fenceposts(0, 0, 3), (1, 0));
        assert_eq!(resolve_fenceposts(0, 0, 0), (0, 0)); // nothing to roll, no fix needed
        assert_eq!(resolve_fenceposts(0, 1, 3), (0, 1)); // already fine
    }

    #[test]
    fn new_rs_scales_up_within_surge_budget() {
        // desired=4, surge=1 => max_total=5, current_total=4 (all old), new=0
        assert_eq!(new_rs_desired_replicas(4, 1, 4, 0), 1);
    }

    #[test]
    fn new_rs_does_not_scale_past_desired() {
        assert_eq!(new_rs_desired_replicas(4, 1, 4, 4), 4);
    }

    #[test]
    fn new_rs_holds_steady_once_at_max_total() {
        assert_eq!(new_rs_desired_replicas(4, 1, 5, 2), 2);
    }

    #[test]
    fn scale_down_budget_respects_min_available() {
        // desired=4, maxUnavailable=1 => minAvailable=3; available_total=4, new unavailable=0
        assert_eq!(scale_down_budget(4, 1, 4, 1, 1), 1);
    }

    #[test]
    fn scale_down_budget_never_negative() {
        assert_eq!(scale_down_budget(4, 0, 2, 4, 1), 0);
    }

    #[test]
    fn scale_down_old_spends_budget_oldest_first() {
        let old = vec![("old-1".to_string(), 2), ("old-2".to_string(), 3)];
        assert_eq!(scale_down_old(old, 3), vec![("old-1".to_string(), 0), ("old-2".to_string(), 2)]);
    }

    #[test]
    fn scale_down_old_skips_already_empty_replica_sets() {
        let old = vec![("empty".to_string(), 0), ("has-some".to_string(), 2)];
        assert_eq!(scale_down_old(old, 5), vec![("has-some".to_string(), 0)]);
    }

    #[test]
    fn template_hash_is_stable_and_distinguishes_templates() {
        let mut t1 = PodTemplateSpec::default();
        t1.metadata = Some(ObjectMeta { labels: Some(BTreeMap::from([("app".to_string(), "web".to_string())])), ..Default::default() });
        let mut t2 = t1.clone();
        t2.metadata = Some(ObjectMeta { labels: Some(BTreeMap::from([("app".to_string(), "web2".to_string())])), ..Default::default() });

        assert_eq!(compute_template_hash(&t1), compute_template_hash(&t1));
        assert_ne!(compute_template_hash(&t1), compute_template_hash(&t2));
    }
}
