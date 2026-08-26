use super::context::{labels, E2eContext};
use anyhow::{Context, Result};
use k8s_openapi::api::batch::v1::{CronJob, Job};
use kube::api::{Api, DeleteParams, PostParams};
use serde_json::json;
use std::time::Duration;

pub(super) async fn job_controller_runs_pods_to_completion(context: &E2eContext) -> Result<()> {
    let name = "job-controller-completion";
    let jobs: Api<Job> = Api::namespaced(context.client.clone(), &context.namespace);
    let job: Job = serde_json::from_value(json!({
        "apiVersion": "batch/v1",
        "kind": "Job",
        "metadata": {"name": name},
        "spec": {
            "completions": 2,
            "parallelism": 2,
            "backoffLimit": 2,
            "template": {"spec": {"restartPolicy": "Never", "containers": [{
                "name": "busybox", "image": "busybox:latest", "command": ["sh", "-c", "exit 0"]
            }]}}
        }
    }))?;
    jobs.create(&PostParams::default(), &job)
        .await
        .context("creating completion Job")?;
    context
        .wait_until("Job to report two succeeded Pods", Duration::from_secs(90), || {
            let jobs = jobs.clone();
            async move {
                Ok(jobs
                    .get(name)
                    .await?
                    .status
                    .and_then(|status| status.succeeded)
                    == Some(2))
            }
        })
        .await?;
    context
        .wait_until("Job Complete=True", Duration::from_secs(30), || {
            let jobs = jobs.clone();
            async move {
                Ok(jobs
                    .get(name)
                    .await?
                    .status
                    .and_then(|status| status.conditions)
                    .unwrap_or_default()
                    .iter()
                    .any(|condition| condition.type_ == "Complete" && condition.status == "True"))
            }
        })
        .await?;
    let _ = jobs.delete(name, &DeleteParams::default()).await;
    Ok(())
}

pub(super) async fn job_controller_fails_after_backoff_limit(
    context: &E2eContext,
) -> Result<()> {
    let name = "job-controller-failure";
    let jobs: Api<Job> = Api::namespaced(context.client.clone(), &context.namespace);
    let job: Job = serde_json::from_value(json!({
        "apiVersion": "batch/v1",
        "kind": "Job",
        "metadata": {"name": name},
        "spec": {
            "backoffLimit": 0,
            "template": {"spec": {"restartPolicy": "Never", "containers": [{
                "name": "busybox", "image": "busybox:latest", "command": ["sh", "-c", "exit 1"]
            }]}}
        }
    }))?;
    jobs.create(&PostParams::default(), &job)
        .await
        .context("creating failing Job")?;
    context
        .wait_until("Job Failed=True", Duration::from_secs(90), || {
            let jobs = jobs.clone();
            async move {
                Ok(jobs
                    .get(name)
                    .await?
                    .status
                    .and_then(|status| status.conditions)
                    .unwrap_or_default()
                    .iter()
                    .any(|condition| condition.type_ == "Failed" && condition.status == "True"))
            }
        })
        .await?;
    let _ = jobs.delete(name, &DeleteParams::default()).await;
    Ok(())
}

pub(super) async fn cronjob_controller_creates_a_job_on_schedule(
    context: &E2eContext,
) -> Result<()> {
    let name = "cronjob-controller";
    let cronjobs: Api<CronJob> = Api::namespaced(context.client.clone(), &context.namespace);
    let jobs: Api<Job> = Api::namespaced(context.client.clone(), &context.namespace);
    let cronjob: CronJob = serde_json::from_value(json!({
        "apiVersion": "batch/v1",
        "kind": "CronJob",
        "metadata": {"name": name},
        "spec": {
            "schedule": "* * * * *",
            "concurrencyPolicy": "Allow",
            "jobTemplate": {"spec": {"template": {"spec": {"restartPolicy": "Never", "containers": [{
                "name": "busybox", "image": "busybox:latest", "command": ["sh", "-c", "exit 0"]
            }]}}}}
        }
    }))?;
    cronjobs
        .create(&PostParams::default(), &cronjob)
        .await
        .context("creating CronJob")?;
    context
        .wait_until("CronJob to create a Job", Duration::from_secs(150), || {
            let jobs = jobs.clone();
            async move {
                Ok(!jobs
                    .list(&labels(&format!("cronjob-name={name}")))
                    .await?
                    .items
                    .is_empty())
            }
        })
        .await?;
    context
        .wait_until("CronJob lastScheduleTime", Duration::from_secs(30), || {
            let cronjobs = cronjobs.clone();
            async move {
                Ok(cronjobs
                    .get(name)
                    .await?
                    .status
                    .and_then(|status| status.last_schedule_time)
                    .is_some())
            }
        })
        .await?;
    let _ = cronjobs.delete(name, &DeleteParams::default()).await;
    let _ = jobs
        .delete_collection(
            &DeleteParams::default(),
            &labels(&format!("cronjob-name={name}")),
        )
        .await;
    Ok(())
}

pub(super) async fn ttl_after_finished_controller_deletes_expired_jobs(
    context: &E2eContext,
) -> Result<()> {
    let name = "job-controller-ttl";
    let jobs: Api<Job> = Api::namespaced(context.client.clone(), &context.namespace);
    let job: Job = serde_json::from_value(json!({
        "apiVersion": "batch/v1",
        "kind": "Job",
        "metadata": {"name": name},
        "spec": {
            "ttlSecondsAfterFinished": 5,
            "backoffLimit": 0,
            "template": {"spec": {"restartPolicy": "Never", "containers": [{
                "name": "busybox", "image": "busybox:latest", "command": ["sh", "-c", "exit 0"]
            }]}}
        }
    }))?;
    jobs.create(&PostParams::default(), &job)
        .await
        .context("creating TTL Job")?;
    context
        .wait_until("TTL Job Complete=True", Duration::from_secs(90), || {
            let jobs = jobs.clone();
            async move {
                Ok(jobs
                    .get(name)
                    .await?
                    .status
                    .and_then(|status| status.conditions)
                    .unwrap_or_default()
                    .iter()
                    .any(|condition| condition.type_ == "Complete" && condition.status == "True"))
            }
        })
        .await?;
    context
        .wait_until(
            "TTL controller to delete the finished Job",
            Duration::from_secs(60),
            || {
                let jobs = jobs.clone();
                async move { Ok(jobs.get_opt(name).await?.is_none()) }
            },
        )
        .await
}
