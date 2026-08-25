use super::context::{labels, E2eContext};
use super::skip_test;
use anyhow::{Context, Result};
use k8s_openapi::api::apps::v1::Deployment;
use k8s_openapi::api::core::v1::Pod;
use kube::api::{Api, DeleteParams, ListParams, PostParams};
use serde_json::json;
use std::process::Command;
use std::time::Duration;

fn needs_cri() -> Result<()> {
    anyhow::ensure!(
        crate::config::Config::from_env()?.nodelet_runtime() == "cri",
        "container cleanup checks require the CRI runtime",
    );
    Ok(())
}

fn ctr_path() -> Option<&'static str> {
    ["/usr/local/bin/ctr", "/usr/bin/ctr", "ctr"]
        .into_iter()
        .find(|path| {
            if path.contains('/') {
                std::path::Path::new(path).is_file()
            } else {
                std::env::var_os("PATH").is_some_and(|paths| {
                    std::env::split_paths(&paths).any(|directory| directory.join(path).is_file())
                })
            }
        })
}

fn containerd_has_container(ctr: &str, id: &str) -> Result<bool> {
    let output = Command::new("sudo")
        .args([ctr, "-n", "k8s.io", "containers", "ls", "-q"])
        .output()
        .with_context(|| format!("running sudo {ctr} -n k8s.io containers ls -q"))?;
    anyhow::ensure!(
        output.status.success(),
        "ctr could not list containerd containers: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .any(|line| line.trim() == id))
}

pub(super) async fn garbage_collector_cascades_deployment_delete_to_replicaset_and_pods(
    context: &E2eContext,
) -> Result<()> {
    let name = "garbage-collector-test";
    let deployments: Api<Deployment> = Api::namespaced(context.client.clone(), &context.namespace);
    let pods: Api<Pod> = Api::namespaced(context.client.clone(), &context.namespace);
    let deployment: Deployment = serde_json::from_value(json!({
        "apiVersion": "apps/v1",
        "kind": "Deployment",
        "metadata": {"name": name},
        "spec": {"replicas": 2, "selector": {"matchLabels": {"app": name}}, "template": {
            "metadata": {"labels": {"app": name}},
            "spec": {"containers": [{"name": "busybox", "image": "busybox:latest", "command": ["sleep", "3600"]}]}
        }}
    }))?;
    deployments
        .create(&PostParams::default(), &deployment)
        .await
        .context("creating garbage collector test Deployment")?;
    context
        .wait_until("garbage collector test Deployment creates a ReplicaSet", Duration::from_secs(60), || {
            let replicasets: Api<k8s_openapi::api::apps::v1::ReplicaSet> =
                Api::namespaced(context.client.clone(), &context.namespace);
            async move {
                Ok(replicasets
                    .list(&labels(&format!("app={name}")))
                    .await?
                    .items
                    .len()
                    == 1)
            }
        })
        .await?;
    context
        .wait_until("garbage collector test Deployment creates two Pods", Duration::from_secs(60), || {
            let pods = pods.clone();
            async move {
                Ok(pods
                    .list(&labels(&format!("app={name}")))
                    .await?
                    .items
                    .len()
                    == 2)
            }
        })
        .await?;
    deployments.delete(name, &DeleteParams::default()).await?;
    context
        .wait_until("garbage collector removes the Deployment ReplicaSet", Duration::from_secs(60), || {
            let replicasets: Api<k8s_openapi::api::apps::v1::ReplicaSet> =
                Api::namespaced(context.client.clone(), &context.namespace);
            async move {
                Ok(replicasets
                    .list(&labels(&format!("app={name}")))
                    .await?
                    .items
                    .is_empty())
            }
        })
        .await?;
    context
        .wait_until("garbage collector removes the Deployment Pods", Duration::from_secs(120), || {
            let pods = pods.clone();
            async move {
                Ok(pods
                    .list(&ListParams::default().labels(&format!("app={name}")))
                    .await?
                    .items
                    .is_empty())
            }
        })
        .await
}

pub(super) async fn pod_teardown_actually_removes_the_sandbox(
    context: &E2eContext,
) -> Result<()> {
    if let Err(error) = needs_cri() {
        return Err(skip_test(error.to_string()));
    }
    let Some(ctr) = ctr_path() else {
        return Err(skip_test("ctr is not installed; container cleanup cannot be verified"));
    };
    let name = "teardown-check";
    let pods: Api<Pod> = Api::namespaced(context.client.clone(), &context.namespace);
    let pod: Pod = serde_json::from_value(json!({
        "apiVersion": "v1",
        "kind": "Pod",
        "metadata": {"name": name},
        "spec": {"containers": [{"name": "app", "image": "busybox:latest", "command": ["sleep", "3600"]}]}
    }))?;
    pods.create(&PostParams::default(), &pod).await?;
    context
        .wait_until("sandbox cleanup Pod to reach Running", Duration::from_secs(90), || {
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
        .await?;
    let container_id = serde_json::to_value(pods.get(name).await?)?
        .pointer("/status/containerStatuses/0/containerID")
        .and_then(|value| value.as_str())
        .and_then(|value| value.strip_prefix("containerd://"))
        .map(str::to_string)
        .context("Running Pod has no containerd container ID")?;
    pods.delete(name, &DeleteParams::default()).await?;
    context
        .wait_until("deleted Pod object to disappear", Duration::from_secs(120), || {
            let pods = pods.clone();
            async move { Ok(pods.get_opt(name).await?.is_none()) }
        })
        .await?;
    context
        .wait_until("containerd container to be removed after Pod deletion", Duration::from_secs(40), || {
            let ctr = ctr.to_string();
            let container_id = container_id.clone();
            async move { Ok(!containerd_has_container(&ctr, &container_id)?) }
        })
        .await
}
