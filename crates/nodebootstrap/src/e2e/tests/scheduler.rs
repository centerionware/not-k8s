use super::context::E2eContext;
use anyhow::{Context, Result};
use k8s_openapi::api::core::v1::{Node, Pod};
use kube::api::{Api, ListParams, Patch, PatchParams, PostParams};
use serde_json::{json, Map, Value};
use std::time::Duration;

async fn first_node(context: &E2eContext) -> Result<Node> {
    Api::<Node>::all(context.client.clone())
        .list(&ListParams::default())
        .await?
        .items
        .into_iter()
        .next()
        .context("the cluster has no Node object")
}

async fn create_pod(context: &E2eContext, name: &str, spec: Value) -> Result<()> {
    let pods: Api<Pod> = Api::namespaced(context.client.clone(), &context.namespace);
    let pod: Pod = serde_json::from_value(json!({
        "apiVersion": "v1",
        "kind": "Pod",
        "metadata": {"name": name},
        "spec": spec
    }))?;
    pods.create(&PostParams::default(), &pod).await?;
    Ok(())
}

async fn pod_is_scheduled(context: &E2eContext, name: &str) -> Result<bool> {
    let pods: Api<Pod> = Api::namespaced(context.client.clone(), &context.namespace);
    Ok(pods.get(name).await?.spec.and_then(|spec| spec.node_name).is_some())
}

pub(super) async fn scheduler_places_an_ordinary_pod(context: &E2eContext) -> Result<()> {
    let name = "scheduler-ordinary";
    create_pod(
        context,
        name,
        json!({"restartPolicy": "Never", "containers": [{"name": "app", "image": "busybox:latest", "command": ["sleep", "30"]}]}),
    )
    .await?;
    context
        .wait_until("ordinary Pod to be scheduled", Duration::from_secs(60), || {
            pod_is_scheduled(context, name)
        })
        .await
}

pub(super) async fn scheduler_honours_a_matching_node_selector(
    context: &E2eContext,
) -> Result<()> {
    let node = first_node(context).await?;
    let node_name = node
        .metadata
        .name
        .clone()
        .context("the Node has no name")?;
    let (key, value) = node
        .metadata
        .labels
        .unwrap_or_default()
        .into_iter()
        .find(|(_, value)| !value.is_empty())
        .context("the Node has no non-empty label for a selector test")?;
    let mut selector = Map::new();
    selector.insert(key, Value::String(value));
    let name = "scheduler-selector-match";
    create_pod(
        context,
        name,
        json!({
            "restartPolicy": "Never",
            "nodeSelector": selector,
            "containers": [{"name": "app", "image": "busybox:latest", "command": ["sleep", "30"]}]
        }),
    )
    .await?;
    let pods: Api<Pod> = Api::namespaced(context.client.clone(), &context.namespace);
    context
        .wait_until("matching-selector Pod to be scheduled", Duration::from_secs(60), || {
            let pods = pods.clone();
            let node_name = node_name.clone();
            async move {
                Ok(pods
                    .get(name)
                    .await?
                    .spec
                    .and_then(|spec| spec.node_name)
                    == Some(node_name))
            }
        })
        .await
}

pub(super) async fn scheduler_leaves_an_impossible_selector_pending(
    context: &E2eContext,
) -> Result<()> {
    let name = "scheduler-selector-no-match";
    let mut selector = Map::new();
    selector.insert(
        "not-k8s-e2e.invalid/never".to_string(),
        Value::String("match".to_string()),
    );
    create_pod(
        context,
        name,
        json!({
            "restartPolicy": "Never",
            "nodeSelector": selector,
            "containers": [{"name": "app", "image": "busybox:latest", "command": ["sleep", "30"]}]
        }),
    )
    .await?;
    let pods: Api<Pod> = Api::namespaced(context.client.clone(), &context.namespace);
    context
        .wait_until("impossible-selector Pod to remain unscheduled", Duration::from_secs(30), || {
            let pods = pods.clone();
            async move { Ok(pods.get(name).await?.spec.and_then(|spec| spec.node_name).is_none()) }
        })
        .await
}

pub(super) async fn scheduler_leaves_a_gated_pod_alone(context: &E2eContext) -> Result<()> {
    let name = "scheduler-gated";
    create_pod(
        context,
        name,
        json!({
            "schedulingGates": [{"name": "example.com/hold"}],
            "containers": [{"name": "app", "image": "busybox:latest", "command": ["sleep", "30"]}]
        }),
    )
    .await?;
    let pods: Api<Pod> = Api::namespaced(context.client.clone(), &context.namespace);
    tokio::time::sleep(Duration::from_secs(5)).await;
    let gated = pods.get(name).await?;
    anyhow::ensure!(
        gated
            .spec
            .as_ref()
            .and_then(|spec| spec.node_name.as_ref())
            .is_none(),
        "a Pod with a scheduling gate was bound before the gate was removed"
    );
    anyhow::ensure!(
        gated
            .spec
            .and_then(|spec| spec.scheduling_gates)
            .is_some_and(|gates| !gates.is_empty()),
        "the scheduler gate disappeared before the test removed it"
    );
    pods.patch(
        name,
        &PatchParams::default(),
        &Patch::Merge(&json!({"spec": {"schedulingGates": null}})),
    )
    .await?;
    context
        .wait_until("ungated Pod to be scheduled", Duration::from_secs(60), || {
            let pods = pods.clone();
            async move { Ok(pods.get(name).await?.spec.and_then(|spec| spec.node_name).is_some()) }
        })
        .await
}

pub(super) async fn scheduler_ignores_a_pod_for_another_scheduler(
    context: &E2eContext,
) -> Result<()> {
    let name = "scheduler-other-scheduler";
    create_pod(
        context,
        name,
        json!({
            "schedulerName": "not-k8s-e2e-other-scheduler",
            "containers": [{"name": "app", "image": "busybox:latest", "command": ["sleep", "30"]}]
        }),
    )
    .await?;
    tokio::time::sleep(Duration::from_secs(5)).await;
    let pods: Api<Pod> = Api::namespaced(context.client.clone(), &context.namespace);
    let pod = pods.get(name).await?;
    anyhow::ensure!(
        pod.spec.and_then(|spec| spec.node_name).is_none(),
        "the configured scheduler bound a Pod assigned to another scheduler"
    );
    Ok(())
}
