use super::context::E2eContext;
use super::skip_test;
use anyhow::Result;
use k8s_openapi::api::core::v1::Pod;
use kube::api::{Api, DeleteParams, PostParams};
use serde_json::json;
use std::time::{Duration, Instant};

fn needs_cri() -> Result<()> {
    anyhow::ensure!(
        crate::config::Config::from_env()?.nodelet_runtime() == "cri",
        "termination concurrency checks require the CRI runtime",
    );
    Ok(())
}

async fn create_slow_pod(context: &E2eContext, pods: &Api<Pod>, name: &str, grace: i64) -> Result<()> {
    let pod: Pod = serde_json::from_value(json!({
        "apiVersion": "v1",
        "kind": "Pod",
        "metadata": {"name": name},
        "spec": {
            "terminationGracePeriodSeconds": grace,
            "containers": [{
                "name": "app",
                "image": "busybox:latest",
                "command": ["sh", "-c", "trap '' TERM; sleep 3600"]
            }]
        }
    }))?;
    pods.create(&PostParams::default(), &pod).await?;
    context
        .wait_until("slow terminating Pod Running", Duration::from_secs(90), || {
            let pods = pods.clone();
            async move {
                Ok(pods
                    .get(name)
                    .await?
                    .status
                    .and_then(|status| status.phase)
                    .as_deref()
                    == Some("Running"))
            }
        })
        .await
}

async fn create_sleep_pod(pods: &Api<Pod>, name: &str) -> Result<()> {
    let pod: Pod = serde_json::from_value(json!({
        "apiVersion": "v1",
        "kind": "Pod",
        "metadata": {"name": name},
        "spec": {
            "containers": [{"name": "app", "image": "busybox:latest", "command": ["sleep", "3600"]}]
        }
    }))?;
    pods.create(&PostParams::default(), &pod).await?;
    Ok(())
}

async fn pod_is_running(pods: &Api<Pod>, name: &str) -> Result<bool> {
    Ok(pods
        .get(name)
        .await?
        .status
        .and_then(|status| status.phase)
        .as_deref()
        == Some("Running"))
}

pub(super) async fn slow_terminating_pod_does_not_stall_another_pods_creation(
    context: &E2eContext,
) -> Result<()> {
    if let Err(error) = needs_cri() {
        return Err(skip_test(error.to_string()));
    }
    let pods: Api<Pod> = Api::namespaced(context.client.clone(), &context.namespace);
    let blocker = "term-blocker";
    let victim = "term-victim";
    let grace = 45_u32;
    create_slow_pod(context, &pods, blocker, grace).await?;
    let started = Instant::now();
    pods.delete(
        blocker,
        &DeleteParams {
            grace_period_seconds: Some(grace),
            ..Default::default()
        },
    )
    .await?;
    context
        .wait_until("slow terminating Pod deletion to begin", Duration::from_secs(30), || {
            let pods = pods.clone();
            async move { Ok(pods.get(blocker).await?.metadata.deletion_timestamp.is_some()) }
        })
        .await?;
    tokio::time::sleep(Duration::from_secs(5)).await;
    create_sleep_pod(&pods, victim).await?;

    context
        .wait_until("unrelated Pod to reach Running during teardown", Duration::from_secs(30), || {
            let pods = pods.clone();
            async move { pod_is_running(&pods, victim).await }
        })
        .await
        .map_err(|error| {
            anyhow::anyhow!(
                "an unrelated Pod could not start while another was terminating; elapsed={}s: {error:#}",
                started.elapsed().as_secs()
            )
        })?;
    anyhow::ensure!(
        started.elapsed() < Duration::from_secs(grace as u64),
        "victim Pod reached Running only after the blocker's {grace}s grace period elapsed"
    );
    Ok(())
}

pub(super) async fn recreated_pod_survives_the_old_pods_detached_teardown(
    context: &E2eContext,
) -> Result<()> {
    if let Err(error) = needs_cri() {
        return Err(skip_test(error.to_string()));
    }
    let pods: Api<Pod> = Api::namespaced(context.client.clone(), &context.namespace);
    let name = "term-recreate";
    let grace = 20_u32;
    create_slow_pod(context, &pods, name, grace).await?;
    pods.delete(
        name,
        &DeleteParams {
            grace_period_seconds: Some(grace),
            ..Default::default()
        },
    )
    .await?;
    tokio::time::sleep(Duration::from_secs(5)).await;
    pods.delete(
        name,
        &DeleteParams {
            grace_period_seconds: Some(0),
            ..Default::default()
        },
    )
    .await?;
    context
        .wait_until("old Pod object to disappear before recreation", Duration::from_secs(30), || {
            let pods = pods.clone();
            async move { Ok(pods.get_opt(name).await?.is_none()) }
        })
        .await?;
    create_sleep_pod(&pods, name).await?;
    context
        .wait_until("replacement Pod to reach Running", Duration::from_secs(90), || {
            let pods = pods.clone();
            async move { pod_is_running(&pods, name).await }
        })
        .await?;
    tokio::time::sleep(Duration::from_secs((grace + 5) as u64)).await;
    anyhow::ensure!(
        pod_is_running(&pods, name).await?,
        "replacement Pod disappeared or left Running after the old detached teardown completed"
    );
    Ok(())
}
