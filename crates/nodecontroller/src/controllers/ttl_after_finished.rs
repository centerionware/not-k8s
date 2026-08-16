//! ttl-after-finished-controller (Group F, batch controllers): deletes a
//! finished (`Complete`/`Failed`) Job once `spec.ttlSecondsAfterFinished`
//! has elapsed since it finished. Poll-driven for the same reason
//! `cronjob-controller` is — "has enough wall-clock time passed since this
//! Job finished" has no watchable event, it's a deadline, and
//! `docs/CONTROLLER_MANAGER.md` scopes this controller to the plain-heap
//! tier (one entry per finished Job, not per node) rather than
//! `wheel.rs`'s timing wheel.
//!
//! Deleting a Job here cascades to its Pods via `garbage-collector-controller`
//! (Group D) — this controller does nothing itself to clean up a Job's
//! Pods, matching upstream's own division of labor (ttl-after-finished only
//! ever deletes the Job object).
//!
//! # Scope of this slice
//!
//! **A flat periodic scan of the cached Job set**, not a per-Job deadline
//! timer — at this controller's expected cardinality (finished Jobs with a
//! TTL set, cluster-wide) a scan every `TICK_PERIOD` is simpler than
//! maintaining one wheel/heap entry per Job and is what
//! `docs/CONTROLLER_MANAGER.md`'s "plain heap tier, not wheel tier" already
//! calls out for this controller — a real heap/wheel would only pay for
//! itself at a cardinality this controller isn't expected to see.

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use futures::StreamExt;
use k8s_openapi::api::batch::v1::Job;
use kube::api::{Api, DeleteParams};
use kube::runtime::watcher::Event;
use kube::{Client, ResourceExt};
use std::collections::HashMap;
use std::time::Duration as StdDuration;

const TICK_PERIOD: StdDuration = StdDuration::from_secs(10);

/// When a finished Job (with a known finish time) becomes eligible for
/// deletion — pure arithmetic, the actual "is it past that time" check is
/// just `now >= this`.
pub fn deletion_deadline(finished_at: DateTime<Utc>, ttl_seconds_after_finished: i64) -> DateTime<Utc> {
    finished_at + chrono::Duration::seconds(ttl_seconds_after_finished.max(0))
}

pub fn is_due(finished_at: DateTime<Utc>, ttl_seconds_after_finished: i64, now: DateTime<Utc>) -> bool {
    now >= deletion_deadline(finished_at, ttl_seconds_after_finished)
}

/// The moment a Job finished — the newer of its `completionTime` and the
/// most recent `Complete`/`Failed` condition's `lastTransitionTime`, since
/// a Failed Job never sets `completionTime` at all (that field is
/// documented as "set on success, and only then").
fn finished_at(job: &Job) -> Option<DateTime<Utc>> {
    let status = job.status.as_ref()?;
    let completion = status.completion_time.as_ref().and_then(crate::k8s_time::to_chrono);
    let condition = status
        .conditions
        .as_ref()
        .into_iter()
        .flatten()
        .filter(|c| (c.type_ == "Complete" || c.type_ == "Failed") && c.status == "True")
        .filter_map(|c| c.last_transition_time.as_ref().and_then(crate::k8s_time::to_chrono))
        .max();
    match (completion, condition) {
        (Some(a), Some(b)) => Some(a.max(b)),
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    }
}

async fn sweep(client: &Client, jobs: &HashMap<String, Job>) {
    let now = Utc::now();
    for job in jobs.values() {
        let Some(ttl) = job.spec.as_ref().and_then(|s| s.ttl_seconds_after_finished) else { continue };
        let Some(finished) = finished_at(job) else { continue };
        if !is_due(finished, ttl as i64, now) {
            continue;
        }
        let namespace = job.namespace().unwrap_or_default();
        let name = job.name_any();
        let api: Api<Job> = Api::namespaced(client.clone(), &namespace);
        match api.delete(&name, &DeleteParams::default()).await {
            Ok(_) => tracing::info!(namespace = %namespace, job = %name, "ttl-after-finished-controller deleted a finished Job past its TTL"),
            Err(kube::Error::Api(ref e)) if e.is_not_found() => {}
            Err(e) => tracing::warn!(namespace = %namespace, job = %name, error = ?e, "failed to delete finished Job past its TTL"),
        }
    }
}

fn ns_of<K: ResourceExt>(obj: &K) -> String {
    obj.namespace().unwrap_or_default()
}

pub async fn run(client: Client, _cfg: &crate::config::Config) -> Result<()> {
    let mut jobs: HashMap<String, Job> = HashMap::new();
    let job_api: Api<Job> = Api::all(client.clone());
    for j in job_api.list(&Default::default()).await.context("listing Jobs to seed ttl-after-finished-controller")?.items {
        jobs.insert(format!("{}/{}", ns_of(&j), j.name_any()), j);
    }

    let mut job_stream = crate::watch::watch_jobs(&client);
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
                    Some(Err(e)) => tracing::warn!(error = ?e, "job watch error in ttl-after-finished-controller"),
                    None => return Ok(()),
                }
            }
            _ = ticker.tick() => {
                sweep(&client, &jobs).await;
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
    fn not_due_before_the_ttl_elapses() {
        assert!(!is_due(dt(0), 60, dt(30)));
    }

    #[test]
    fn due_exactly_at_the_deadline() {
        assert!(is_due(dt(0), 60, dt(60)));
    }

    #[test]
    fn due_well_after_the_deadline() {
        assert!(is_due(dt(0), 60, dt(1000)));
    }

    #[test]
    fn a_zero_ttl_is_due_immediately() {
        assert!(is_due(dt(0), 0, dt(0)));
    }
}
