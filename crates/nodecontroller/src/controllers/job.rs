//! job-controller (Group F, batch controllers): runs a Job's Pods to
//! completion — creates up to `spec.parallelism` Pods, counts terminal
//! ones, and reports `Complete`/`Failed` once the Job's target is reached
//! or `backoffLimit` is exceeded. Pure event — a Job's actual vs. desired
//! Pod count changes exactly when a Pod or the Job itself changes, nothing
//! to poll (unlike `cronjob-controller`, which genuinely needs a clock).
//!
//! # Scope of this slice
//!
//! **`NonIndexed` completion mode only.** `spec.completionMode: Indexed`
//! (per-index completion tracking, `batch.kubernetes.io/job-completion-index`
//! Pod annotations, `completedIndexes`/`failedIndexes` status) is real
//! complexity this slice doesn't take on — a Job with `completionMode:
//! Indexed` is logged once and left alone rather than mismanaged.
//!
//! **No `podFailurePolicy` or `successPolicy`.** Both are opt-in fields
//! most Jobs never set; when set here, they're silently ignored rather than
//! honored — a real, named gap, not a crash.
//!
//! **No `activeDeadlineSeconds`.** Enforcing it needs a poll timer (the
//! deadline isn't tied to any watchable event), which this controller
//! deliberately doesn't have — see the module doc's opening paragraph. A
//! Job with `activeDeadlineSeconds` set simply never times out here.
//!
//! **Completion/failure counts are recomputed from the live Pod set every
//! reconcile, not accumulated via upstream's `uncountedTerminatedPods`
//! bookkeeping.** This works because — matching upstream's own behavior —
//! this controller never deletes a terminal (Succeeded/Failed) Pod itself,
//! so `succeeded`/`failed` are always exactly "how many owned Pods are
//! currently in that phase," no separate counter to keep in sync. Simpler,
//! not less correct, for the cases this slice covers.
//!
//! **`managedBy` is honored as a skip, not a full hand-off protocol**: a
//! Job whose `spec.managedBy` names something other than the reserved
//! `kubernetes.io/job-controller` value is left alone entirely (upstream's
//! own escape hatch for an external Job controller), same as `Indexed` mode.
//!
//! **`ttlSecondsAfterFinished` is a separate controller**
//! (`ttl-after-finished-controller`, this same group) — not handled here.
//!
//! **Two apiserver admission invariants this controller must satisfy even
//! though it doesn't implement the features behind them**, both confirmed
//! live in CI (k3s 1.33's `JobValidation` admission plugin, real `422`
//! responses, not documentation): a finished Job's `status.active` must be
//! `0` (this reconcile reports the *post-outcome* count once an outcome is
//! decided, not the live snapshot that decided it), and `Complete=True`
//! requires a `SuccessCriteriaMet=True` condition alongside it (a
//! `successPolicy`-era invariant; both conditions are set together even
//! though `successPolicy` itself isn't implemented here — the same
//! `FailureTarget=True` condition is required alongside `Failed=True` on
//! the failure path, confirmed live in CI the same way). A third: the
//! apiserver adds a `batch.kubernetes.io/job-tracking` finalizer to every
//! Job at creation regardless of which controller-manager is running, and
//! a Job can never actually be deleted (by `ttl-after-finished-controller`
//! or anyone else) until that finalizer is removed — this controller
//! strips it once a Job reaches a terminal outcome, standing in for
//! upstream's real removal condition ("finished accounting via
//! `uncountedTerminatedPods`") with this crate's simpler equivalent
//! ("reconcile decided the outcome").

use anyhow::{Context, Result};
use futures::StreamExt;
use k8s_openapi::api::batch::v1::{Job, JobCondition};
use k8s_openapi::api::core::v1::Pod;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{ObjectMeta, OwnerReference};
use kube::api::{Api, Patch, PatchParams, PostParams};
use kube::runtime::watcher::Event;
use kube::{Client, ResourceExt};
use std::collections::HashMap;

const RESERVED_MANAGED_BY: &str = "kubernetes.io/job-controller";
const JOB_TRACKING_FINALIZER: &str = "batch.kubernetes.io/job-tracking";

/// How many new Pods to create this reconcile — 0 once the Job has already
/// met its target, capped by both `parallelism` and (when set) how much of
/// `completions` remains outstanding.
pub fn pods_to_create(parallelism: i32, completions: Option<i32>, active: i32, succeeded: i32) -> i32 {
    let mut room = (parallelism - active).max(0);
    if let Some(completions) = completions {
        let remaining = (completions - succeeded - active).max(0);
        room = room.min(remaining);
    }
    room.max(0)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Complete,
    Failed,
}

/// Whether the Job has reached a terminal outcome given its current
/// counts. `None` means "still running" — pure decision, the reconcile
/// loop just acts on it.
pub fn job_outcome(succeeded: i32, failed: i32, completions: Option<i32>, backoff_limit: i32) -> Option<Outcome> {
    let target_met = match completions {
        Some(c) => succeeded >= c,
        None => succeeded >= 1, // worker-pool pattern: any success is the Job's success
    };
    if target_met {
        return Some(Outcome::Complete);
    }
    if failed > backoff_limit {
        return Some(Outcome::Failed);
    }
    None
}

fn owned_by(pod: &Pod, job_uid: &str) -> bool {
    pod.metadata.owner_references.as_ref().into_iter().flatten().any(|o| o.controller == Some(true) && o.uid == job_uid)
}

fn owner_reference(job: &Job) -> OwnerReference {
    OwnerReference {
        api_version: "batch/v1".to_string(),
        kind: "Job".to_string(),
        name: job.name_any(),
        uid: job.uid().unwrap_or_default(),
        controller: Some(true),
        block_owner_deletion: Some(true),
        ..Default::default()
    }
}

fn build_pod(job: &Job, name: &str) -> Pod {
    let template = job.spec.as_ref().map(|s| s.template.clone()).unwrap_or_default();
    let mut labels = template.metadata.as_ref().and_then(|m| m.labels.clone()).unwrap_or_default();
    labels.entry("job-name".to_string()).or_insert_with(|| job.name_any());
    let annotations = template.metadata.as_ref().and_then(|m| m.annotations.clone());
    Pod {
        metadata: ObjectMeta {
            name: Some(name.to_string()),
            namespace: job.namespace(),
            labels: Some(labels),
            annotations,
            owner_references: Some(vec![owner_reference(job)]),
            ..Default::default()
        },
        spec: template.spec,
        ..Default::default()
    }
}

fn condition(type_: &str, message: &str) -> JobCondition {
    let now = crate::k8s_time::from_chrono(crate::k8s_time::now());
    JobCondition {
        type_: type_.to_string(),
        status: "True".to_string(),
        last_probe_time: Some(now.clone()),
        last_transition_time: Some(now),
        message: Some(message.to_string()),
        reason: None,
    }
}

fn skip_job(job: &Job) -> bool {
    let indexed = job.spec.as_ref().and_then(|s| s.completion_mode.as_deref()) == Some("Indexed");
    let foreign_manager = job
        .spec
        .as_ref()
        .and_then(|s| s.managed_by.as_deref())
        .is_some_and(|m| m != RESERVED_MANAGED_BY);
    indexed || foreign_manager
}

async fn reconcile_job(client: &Client, namespace: &str, name: &str, pod_cache: &HashMap<String, Pod>) {
    let job_api: Api<Job> = Api::namespaced(client.clone(), namespace);
    let pod_api: Api<Pod> = Api::namespaced(client.clone(), namespace);

    let job = match job_api.get_opt(name).await {
        Ok(Some(j)) => j,
        Ok(None) => return, // gone — its Pods are garbage-collector-controller's job
        Err(e) => {
            tracing::warn!(namespace = %namespace, job = %name, error = ?e, "failed to read Job for reconcile");
            return;
        }
    };
    if skip_job(&job) {
        return;
    }
    let Some(job_uid) = job.uid() else { return };
    let spec = job.spec.clone().unwrap_or_default();

    let owned: Vec<&Pod> =
        pod_cache.values().filter(|p| p.namespace().as_deref() == Some(namespace)).filter(|p| owned_by(p, &job_uid)).collect();
    let live: Vec<&&Pod> = owned.iter().filter(|p| p.metadata.deletion_timestamp.is_none()).collect();

    let succeeded = owned.iter().filter(|p| p.status.as_ref().and_then(|s| s.phase.as_deref()) == Some("Succeeded")).count() as i32;
    let failed = owned.iter().filter(|p| p.status.as_ref().and_then(|s| s.phase.as_deref()) == Some("Failed")).count() as i32;
    let active = live
        .iter()
        .filter(|p| !matches!(p.status.as_ref().and_then(|s| s.phase.as_deref()), Some("Succeeded") | Some("Failed")))
        .count() as i32;

    let backoff_limit = spec.backoff_limit.unwrap_or(6);
    let suspend = spec.suspend.unwrap_or(false);
    let outcome = job_outcome(succeeded, failed, spec.completions, backoff_limit);
    let already_terminal = job.status.as_ref().and_then(|s| s.conditions.as_ref()).into_iter().flatten().any(|c| {
        (c.type_ == "Complete" || c.type_ == "Failed") && c.status == "True"
    });

    if outcome.is_none() && !suspend && !already_terminal {
        let parallelism = spec.parallelism.unwrap_or(1);
        // Deterministic names, not build_pod's random suffix directly:
        // two concurrent reconciles (a Job event and a Pod event racing
        // each other, both reading a `pod_cache` that hasn't yet observed
        // the other's in-flight create) both computing "1 more Pod needed"
        // must land on the *same* name, so the loser's create fails
        // AlreadyExists instead of both succeeding and overshooting
        // parallelism — the exact bug this project already hit once with
        // ReplicaSet's `generateName` (see deployment-controller's own
        // history) and is avoiding here from the start.
        let base = owned.len() as i32;
        for i in 0..pods_to_create(parallelism, spec.completions, active, succeeded) {
            let pod_name = format!("{name}-{}", base + i);
            let pod = build_pod(&job, &pod_name);
            match pod_api.create(&PostParams::default(), &pod).await {
                Ok(_) => {}
                Err(kube::Error::Api(ref e)) if e.is_already_exists() => {}
                Err(e) => {
                    tracing::warn!(namespace = %namespace, job = %name, pod = %pod_name, error = ?e, "failed to create Pod for Job");
                }
            }
        }
    } else if suspend && !already_terminal {
        // Suspending resets the active generation: delete every live Pod.
        for pod in &live {
            if let Err(e) = pod_api.delete(&pod.name_any(), &Default::default()).await {
                tracing::warn!(namespace = %namespace, job = %name, pod = %pod.name_any(), error = ?e, "failed to delete Pod for suspended Job");
            }
        }
    }

    let mut status = job.status.clone().unwrap_or_default();
    // The apiserver's JobValidation admission rejects a finished Job
    // (Complete or Failed) reporting any Pod still active — confirmed live
    // in CI (a real `422 active>0 is invalid for finished job` response):
    // once this reconcile knows the outcome, report the *post-outcome*
    // active count (0), not the live snapshot that outcome was computed
    // from.
    let terminal_now = already_terminal || outcome.is_some();
    status.active = Some(if suspend || terminal_now { 0 } else { active });
    status.succeeded = Some(succeeded);
    status.failed = Some(failed);
    if status.start_time.is_none() && !suspend {
        status.start_time = Some(crate::k8s_time::from_chrono(crate::k8s_time::now()));
    }
    if !already_terminal {
        if let Some(o) = outcome {
            let mut conditions = status.conditions.clone().unwrap_or_default();
            if o == Outcome::Complete {
                // The apiserver's JobValidation admission also rejects
                // Complete=True without a SuccessCriteriaMet=True
                // condition already present (confirmed live in CI) — a
                // newer batch/v1 invariant tying Complete to the
                // success-policy machinery even though this controller
                // doesn't implement `successPolicy` itself (see module
                // doc); satisfying the admission rule just means setting
                // both conditions together.
                conditions.push(condition("SuccessCriteriaMet", "Job reached its completion target"));
                conditions.push(condition("Complete", "Job reached its completion target"));
                status.completion_time = Some(crate::k8s_time::from_chrono(crate::k8s_time::now()));
            } else {
                // Same JobValidation admission invariant as Complete/
                // SuccessCriteriaMet above, mirrored for the failure path
                // — confirmed live in CI: `Failed=True` was rejected
                // without a `FailureTarget=True` condition present too.
                conditions.push(condition("FailureTarget", "Job exceeded its backoffLimit"));
                conditions.push(condition("Failed", "Job exceeded its backoffLimit"));
            }
            status.conditions = Some(conditions);
        }
    }

    let status_matches = job.status.as_ref() == Some(&status);
    if !status_matches {
        let patch = serde_json::json!({ "status": status });
        if let Err(e) = job_api.patch_status(name, &PatchParams::default(), &Patch::Merge(&patch)).await {
            tracing::warn!(namespace = %namespace, job = %name, error = ?e, "failed to patch Job status");
        }
    }

    // The apiserver adds `batch.kubernetes.io/job-tracking` to every Job at
    // creation (JobTrackingWithFinalizers, GA since 1.26 — apiserver-side,
    // not something a controller opts into). Upstream's own job-controller
    // strips it once it has finished accounting a Job's terminated Pods via
    // `uncountedTerminatedPods`; this controller doesn't do that
    // bookkeeping (see module doc), but still must remove the finalizer
    // once terminal, or the Job can never actually be deleted by anyone —
    // confirmed live in CI: `ttl-after-finished-controller` "succeeded" at
    // deleting a finished Job every tick forever, because a delete with a
    // finalizer still present only sets `deletionTimestamp` and never
    // actually removes the object.
    if terminal_now {
        if let Some(finalizers) = &job.metadata.finalizers {
            if finalizers.iter().any(|f| f == JOB_TRACKING_FINALIZER) {
                let remaining: Vec<&String> = finalizers.iter().filter(|f| *f != JOB_TRACKING_FINALIZER).collect();
                let patch = serde_json::json!({ "metadata": { "finalizers": remaining } });
                if let Err(e) = job_api.patch(name, &PatchParams::default(), &Patch::Merge(&patch)).await {
                    tracing::warn!(namespace = %namespace, job = %name, error = ?e, "failed to strip job-tracking finalizer from finished Job");
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
    let mut jobs: std::collections::HashSet<(String, String)> = std::collections::HashSet::new();

    let pod_api: Api<Pod> = Api::all(client.clone());
    let job_api: Api<Job> = Api::all(client.clone());

    for p in pod_api.list(&Default::default()).await.context("listing Pods to seed job-controller")?.items {
        pods.insert(format!("{}/{}", ns_of(&p), p.name_any()), p);
    }
    for j in job_api.list(&Default::default()).await.context("listing Jobs to seed job-controller")?.items {
        let ns = ns_of(&j);
        let name = j.name_any();
        jobs.insert((ns.clone(), name.clone()));
        reconcile_job(&client, &ns, &name, &pods).await;
    }

    let mut pod_stream = crate::watch::watch_pods(&client);
    let mut job_stream = crate::watch::watch_jobs(&client);

    loop {
        tokio::select! {
            ev = pod_stream.next() => {
                match ev {
                    Some(Ok(Event::Apply(pod))) | Some(Ok(Event::InitApply(pod))) => {
                        let ns = ns_of(&pod);
                        pods.insert(format!("{ns}/{}", pod.name_any()), pod);
                        for (job_ns, job_name) in jobs.iter().filter(|(n, _)| *n == ns) {
                            reconcile_job(&client, job_ns, job_name, &pods).await;
                        }
                    }
                    Some(Ok(Event::Delete(pod))) => {
                        let ns = ns_of(&pod);
                        pods.remove(&format!("{ns}/{}", pod.name_any()));
                        for (job_ns, job_name) in jobs.iter().filter(|(n, _)| *n == ns) {
                            reconcile_job(&client, job_ns, job_name, &pods).await;
                        }
                    }
                    Some(Ok(Event::Init | Event::InitDone)) => {}
                    Some(Err(e)) => tracing::warn!(error = ?e, "pod watch error in job-controller"),
                    None => return Ok(()),
                }
            }
            ev = job_stream.next() => {
                match ev {
                    Some(Ok(Event::Apply(job))) | Some(Ok(Event::InitApply(job))) => {
                        let ns = ns_of(&job);
                        let name = job.name_any();
                        jobs.insert((ns.clone(), name.clone()));
                        reconcile_job(&client, &ns, &name, &pods).await;
                    }
                    Some(Ok(Event::Delete(job))) => {
                        jobs.remove(&(ns_of(&job), job.name_any()));
                    }
                    Some(Ok(Event::Init | Event::InitDone)) => {}
                    Some(Err(e)) => tracing::warn!(error = ?e, "job watch error in job-controller"),
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
    fn creates_up_to_parallelism() {
        assert_eq!(pods_to_create(3, None, 0, 0), 3);
        assert_eq!(pods_to_create(3, None, 2, 0), 1);
        assert_eq!(pods_to_create(3, None, 3, 0), 0);
    }

    #[test]
    fn caps_at_remaining_completions() {
        assert_eq!(pods_to_create(5, Some(2), 0, 0), 2);
        assert_eq!(pods_to_create(5, Some(2), 1, 0), 1);
        assert_eq!(pods_to_create(5, Some(2), 0, 1), 1);
        assert_eq!(pods_to_create(5, Some(2), 0, 2), 0);
    }

    #[test]
    fn never_negative() {
        assert_eq!(pods_to_create(1, Some(1), 5, 5), 0);
    }

    #[test]
    fn worker_pool_completes_on_first_success() {
        assert_eq!(job_outcome(1, 0, None, 6), Some(Outcome::Complete));
        assert_eq!(job_outcome(0, 0, None, 6), None);
    }

    #[test]
    fn fixed_completions_need_the_full_count() {
        assert_eq!(job_outcome(2, 0, Some(3), 6), None);
        assert_eq!(job_outcome(3, 0, Some(3), 6), Some(Outcome::Complete));
    }

    #[test]
    fn failure_over_backoff_limit_fails_the_job() {
        assert_eq!(job_outcome(0, 6, Some(3), 6), None);
        assert_eq!(job_outcome(0, 7, Some(3), 6), Some(Outcome::Failed));
    }

    #[test]
    fn completion_wins_over_a_simultaneous_failure_count() {
        // Reaching the target takes priority even if failures also crossed
        // the limit in the same reconcile — matches upstream: a Job that
        // has enough successes is Complete, full stop.
        assert_eq!(job_outcome(3, 10, Some(3), 6), Some(Outcome::Complete));
    }
}
