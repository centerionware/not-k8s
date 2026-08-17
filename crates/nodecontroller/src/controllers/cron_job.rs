//! cronjob-controller (Group F, batch controllers): creates a Job from
//! `spec.jobTemplate` each time `spec.schedule` comes due. The one
//! genuinely poll-driven controller in this group — "has the clock passed
//! a cron boundary" has no watchable event to subscribe to, matching
//! `docs/CONTROLLER_MANAGER.md`'s "absence/deadline detection is
//! irreducible polling" framing. Scoped to the plain-heap tier that doc
//! describes (low cardinality — one entry per CronJob, not per node), so
//! this is a flat periodic scan, not `wheel.rs`'s timing wheel — see
//! `TICK_PERIOD` below.
//!
//! Schedule parsing/next-run math is `crate::cron_schedule` — read that
//! module's own doc for its scope.
//!
//! # Scope of this slice
//!
//! **One missed run is caught up, not every missed run.** If nodecontroller
//! was down (or the CronJob suspended) across several schedule boundaries,
//! this reconciles from `lastScheduleTime`/`creationTimestamp` forward to
//! the single next boundary that's now due, creates that one Job, and
//! catches the next boundary on a later tick — it does not attempt to
//! backfill every boundary that was missed. Upstream's own behavior here is
//! itself bounded (a `100`-missed-schedules cutoff before it gives up and
//! logs rather than truly backfilling), so this is a difference of degree,
//! not of kind.
//!
//! **`startingDeadlineSeconds` is honored as "skip if too late", not
//! tracked per-missed-occurrence.** A due run older than the deadline is
//! simply not created; `lastScheduleTime` still advances past it so the
//! next tick evaluates the *next* boundary, not the same stale one forever.
//!
//! **`timeZone` is not honored** — every schedule is evaluated in UTC
//! regardless of `spec.timeZone`. A real, named gap: a CronJob asking for
//! a non-UTC zone will run at the wrong wall-clock time in that zone.
//!
//! **History limits prune by Job name lexical order among the terminal set
//! this controller can see**, not upstream's exact "oldest completion time
//! first" tie-break refinement — close enough for the common case (Jobs
//! are lexically ordered by creation via their random-suffix names only
//! coincidentally, so this reconciles the *count* correctly and picks a
//! reasonable, if not byte-identical, survivor set).

use crate::cron_schedule::Schedule;
use anyhow::Result;
use chrono::{DateTime, Utc};
use futures::StreamExt;
use k8s_openapi::api::batch::v1::{CronJob, Job};
use k8s_openapi::api::core::v1::ObjectReference;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{ObjectMeta, OwnerReference};
use kube::api::{Api, Patch, PatchParams, PostParams};
use kube::runtime::watcher::Event;
use kube::{Client, ResourceExt};
use std::collections::HashMap;
use std::time::Duration as StdDuration;

const TICK_PERIOD: StdDuration = StdDuration::from_secs(10);

/// Is `scheduled` still creatable given `now` and an optional
/// `startingDeadlineSeconds`? `None` deadline means "always", matching
/// upstream's own default of unlimited catch-up.
pub fn within_deadline(scheduled: DateTime<Utc>, now: DateTime<Utc>, starting_deadline_seconds: Option<i64>) -> bool {
    match starting_deadline_seconds {
        None => true,
        Some(s) => (now - scheduled).num_seconds() <= s.max(0),
    }
}

/// Which of `terminal` (name, most-recent-first is NOT assumed — caller
/// sorts) to delete so at most `limit` remain. `limit <= 0` keeps
/// everything (upstream: unset means "no limit", and this crate treats a
/// non-positive configured limit the same way rather than deleting
/// everything).
pub fn jobs_to_prune(mut terminal: Vec<String>, limit: i32) -> Vec<String> {
    if limit <= 0 {
        return Vec::new();
    }
    terminal.sort();
    let keep = limit as usize;
    if terminal.len() <= keep {
        return Vec::new();
    }
    terminal.drain(terminal.len() - keep..);
    terminal
}

fn owner_reference(cj: &CronJob) -> OwnerReference {
    OwnerReference {
        api_version: "batch/v1".to_string(),
        kind: "CronJob".to_string(),
        name: cj.name_any(),
        uid: cj.uid().unwrap_or_default(),
        controller: Some(true),
        block_owner_deletion: Some(true),
        ..Default::default()
    }
}

fn owned_by(job: &Job, cj_uid: &str) -> bool {
    job.metadata.owner_references.as_ref().into_iter().flatten().any(|o| o.controller == Some(true) && o.uid == cj_uid)
}

fn job_is_terminal(job: &Job) -> bool {
    job.status.as_ref().and_then(|s| s.conditions.as_ref()).into_iter().flatten().any(|c| {
        (c.type_ == "Complete" || c.type_ == "Failed") && c.status == "True"
    })
}

fn build_job(cj: &CronJob, scheduled: DateTime<Utc>) -> Job {
    let template = cj.spec.as_ref().map(|s| s.job_template.clone()).unwrap_or_default();
    let name = format!("{}-{}", cj.name_any(), scheduled.timestamp());
    let mut labels = template.metadata.as_ref().and_then(|m| m.labels.clone()).unwrap_or_default();
    labels.entry("cronjob-name".to_string()).or_insert_with(|| cj.name_any());
    let annotations = template.metadata.as_ref().and_then(|m| m.annotations.clone());
    Job {
        metadata: ObjectMeta {
            name: Some(name),
            namespace: cj.namespace(),
            labels: Some(labels),
            annotations,
            owner_references: Some(vec![owner_reference(cj)]),
            ..Default::default()
        },
        spec: template.spec,
        ..Default::default()
    }
}

async fn reconcile_cron_job(client: &Client, namespace: &str, name: &str, job_cache: &HashMap<String, Job>) {
    let cj_api: Api<CronJob> = Api::namespaced(client.clone(), namespace);
    let job_api: Api<Job> = Api::namespaced(client.clone(), namespace);

    let cj = match cj_api.get_opt(name).await {
        Ok(Some(c)) => c,
        Ok(None) => return, // gone — its Jobs are garbage-collector-controller's job
        Err(e) => {
            tracing::warn!(namespace = %namespace, cronjob = %name, error = ?e, "failed to read CronJob for reconcile");
            return;
        }
    };
    let Some(cj_uid) = cj.uid() else { return };
    let spec = cj.spec.clone().unwrap_or_default();

    let owned: Vec<&Job> = job_cache
        .values()
        .filter(|j| j.namespace().as_deref() == Some(namespace))
        .filter(|j| owned_by(j, &cj_uid))
        .collect();
    let active: Vec<&&Job> = owned.iter().filter(|j| !job_is_terminal(j)).collect();

    let now = Utc::now();
    let mut status = cj.status.clone().unwrap_or_default();

    if !spec.suspend.unwrap_or(false) {
        if let Ok(schedule) = Schedule::parse(&spec.schedule) {
            let base = status
                .last_schedule_time
                .as_ref()
                .and_then(crate::k8s_time::to_chrono)
                .or_else(|| cj.creation_timestamp().as_ref().and_then(crate::k8s_time::to_chrono))
                .unwrap_or(now);
            if let Some(next) = schedule.next_after(base) {
                if next <= now {
                    let create = if !active.is_empty() {
                        match spec.concurrency_policy.as_deref() {
                            Some("Forbid") => false,
                            Some("Replace") => {
                                for job in &active {
                                    let _ = job_api.delete(&job.name_any(), &Default::default()).await;
                                }
                                true
                            }
                            _ => true, // "Allow", the default
                        }
                    } else {
                        true
                    };
                    if create && within_deadline(next, now, spec.starting_deadline_seconds) {
                        let job = build_job(&cj, next);
                        match job_api.create(&PostParams::default(), &job).await {
                            Ok(_) => {
                                status.last_schedule_time = Some(crate::k8s_time::from_chrono(next));
                            }
                            Err(kube::Error::Api(ref e)) if e.is_already_exists() => {
                                // Already created (e.g. a previous reconcile
                                // raced this one) — still advance the
                                // watermark so we don't retry forever.
                                status.last_schedule_time = Some(crate::k8s_time::from_chrono(next));
                            }
                            Err(e) => {
                                tracing::warn!(namespace = %namespace, cronjob = %name, error = ?e, "failed to create Job for CronJob");
                            }
                        }
                    } else if create {
                        // Due, but past startingDeadlineSeconds — record it
                        // as handled so the next tick moves on to the
                        // following boundary instead of re-evaluating this
                        // same missed one forever.
                        status.last_schedule_time = Some(crate::k8s_time::from_chrono(next));
                    }
                    // Forbid-with-an-active-run: deliberately do NOT
                    // advance last_schedule_time, so the same due boundary
                    // is retried once the active Job finishes.
                }
            }
        } else {
            tracing::warn!(namespace = %namespace, cronjob = %name, schedule = %spec.schedule, "CronJob has an unparseable schedule");
        }
    }

    status.active = if active.is_empty() {
        None
    } else {
        Some(active.iter().map(|j| ObjectReference { name: Some(j.name_any()), namespace: j.namespace(), ..Default::default() }).collect())
    };
    // lastSuccessfulTime: the newest completionTime among owned Jobs that
    // reached Complete, if any — a plain max, not incremental bookkeeping,
    // since the owned-Job cache is already the full live picture.
    if let Some(newest) = owned
        .iter()
        .filter(|j| j.status.as_ref().and_then(|s| s.conditions.as_ref()).into_iter().flatten().any(|c| c.type_ == "Complete" && c.status == "True"))
        .filter_map(|j| j.status.as_ref().and_then(|s| s.completion_time.as_ref()).and_then(crate::k8s_time::to_chrono))
        .max()
    {
        status.last_successful_time = Some(crate::k8s_time::from_chrono(newest));
    }

    if cj.status.as_ref() != Some(&status) {
        let patch = serde_json::json!({ "status": status });
        if let Err(e) = cj_api.patch_status(name, &PatchParams::default(), &Patch::Merge(&patch)).await {
            tracing::warn!(namespace = %namespace, cronjob = %name, error = ?e, "failed to patch CronJob status");
        }
    }

    // History limits: prune terminal owned Jobs beyond the configured
    // count, split by outcome (upstream tracks these independently).
    let succeeded: Vec<String> = owned
        .iter()
        .filter(|j| j.status.as_ref().and_then(|s| s.conditions.as_ref()).into_iter().flatten().any(|c| c.type_ == "Complete" && c.status == "True"))
        .map(|j| j.name_any())
        .collect();
    let failed: Vec<String> = owned
        .iter()
        .filter(|j| j.status.as_ref().and_then(|s| s.conditions.as_ref()).into_iter().flatten().any(|c| c.type_ == "Failed" && c.status == "True"))
        .map(|j| j.name_any())
        .collect();
    for name_to_delete in jobs_to_prune(succeeded, spec.successful_jobs_history_limit.unwrap_or(3))
        .into_iter()
        .chain(jobs_to_prune(failed, spec.failed_jobs_history_limit.unwrap_or(1)))
    {
        if let Err(e) = job_api.delete(&name_to_delete, &Default::default()).await {
            tracing::warn!(namespace = %namespace, cronjob = %name, job = %name_to_delete, error = ?e, "failed to prune old Job for CronJob history limit");
        }
    }
}

fn ns_of<K: ResourceExt>(obj: &K) -> String {
    obj.namespace().unwrap_or_default()
}

pub async fn run(client: Client, _cfg: &crate::config::Config) -> Result<()> {
    let mut jobs: HashMap<String, Job> = HashMap::new();
    let mut cron_jobs: std::collections::HashSet<(String, String)> = std::collections::HashSet::new();

    let mut job_stream = crate::watch::watch_jobs(&client);
    let mut cj_stream = crate::watch::watch_cron_jobs(&client);
    let mut ticker = tokio::time::interval(TICK_PERIOD);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            ev = job_stream.next() => {
                match ev {
                    Some(Ok(Event::Apply(job))) | Some(Ok(Event::InitApply(job))) => {
                        let ns = ns_of(&job);
                        jobs.insert(format!("{ns}/{}", job.name_any()), job);
                    }
                    Some(Ok(Event::Delete(job))) => {
                        let ns = ns_of(&job);
                        jobs.remove(&format!("{ns}/{}", job.name_any()));
                    }
                    Some(Ok(Event::Init | Event::InitDone)) => {}
                    Some(Err(e)) => tracing::warn!(error = ?e, "job watch error in cronjob-controller"),
                    None => return Ok(()),
                }
            }
            ev = cj_stream.next() => {
                match ev {
                    Some(Ok(Event::Apply(cj))) | Some(Ok(Event::InitApply(cj))) => {
                        let ns = ns_of(&cj);
                        let name = cj.name_any();
                        cron_jobs.insert((ns.clone(), name.clone()));
                        reconcile_cron_job(&client, &ns, &name, &jobs).await;
                    }
                    Some(Ok(Event::Delete(cj))) => {
                        cron_jobs.remove(&(ns_of(&cj), cj.name_any()));
                    }
                    Some(Ok(Event::Init | Event::InitDone)) => {}
                    Some(Err(e)) => tracing::warn!(error = ?e, "cronjob watch error in cronjob-controller"),
                    None => return Ok(()),
                }
            }
            _ = ticker.tick() => {
                for (ns, name) in cron_jobs.clone() {
                    reconcile_cron_job(&client, &ns, &name, &jobs).await;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn dt(secs: i64) -> DateTime<Utc> {
        Utc.timestamp_opt(secs, 0).unwrap()
    }

    #[test]
    fn no_deadline_always_allows() {
        assert!(within_deadline(dt(0), dt(100_000), None));
    }

    #[test]
    fn deadline_rejects_a_too_stale_schedule() {
        assert!(within_deadline(dt(0), dt(30), Some(60)));
        assert!(!within_deadline(dt(0), dt(90), Some(60)));
    }

    #[test]
    fn pruning_keeps_the_most_recent_by_name_and_deletes_the_rest() {
        let names = vec!["cj-1".to_string(), "cj-2".to_string(), "cj-3".to_string()];
        assert_eq!(jobs_to_prune(names, 2), vec!["cj-1".to_string()]);
    }

    #[test]
    fn pruning_is_a_noop_under_the_limit() {
        let names = vec!["cj-1".to_string()];
        assert_eq!(jobs_to_prune(names, 3), Vec::<String>::new());
    }

    #[test]
    fn a_non_positive_limit_keeps_everything() {
        let names = vec!["cj-1".to_string(), "cj-2".to_string()];
        assert_eq!(jobs_to_prune(names, 0), Vec::<String>::new());
    }
}
