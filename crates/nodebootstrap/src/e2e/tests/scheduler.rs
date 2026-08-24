use super::context::E2eContext;
use super::skip_test;
use anyhow::{Context, Result};
use k8s_openapi::api::core::v1::{Node, Pod};
use kube::api::{Api, DeleteParams, ListParams, Patch, PatchParams, PostParams};
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

async fn create_labeled_pod(
    context: &E2eContext,
    name: &str,
    labels: Value,
    spec: Value,
) -> Result<()> {
    let pods: Api<Pod> = Api::namespaced(context.client.clone(), &context.namespace);
    let pod: Pod = serde_json::from_value(json!({
        "apiVersion": "v1",
        "kind": "Pod",
        "metadata": {"name": name, "labels": labels},
        "spec": spec
    }))?;
    pods.create(&PostParams::default(), &pod).await?;
    Ok(())
}

async fn pod_is_scheduled(context: &E2eContext, name: &str) -> Result<bool> {
    let pods: Api<Pod> = Api::namespaced(context.client.clone(), &context.namespace);
    Ok(pods.get(name).await?.spec.and_then(|spec| spec.node_name).is_some())
}

async fn require_single_node(context: &E2eContext) -> Result<()> {
    let nodes: Api<Node> = Api::all(context.client.clone());
    let count = nodes.list(&ListParams::default()).await?.items.len();
    if count != 1 {
        return Err(skip_test(format!(
            "scheduler topology checks require one node, found {count}"
        )));
    }
    Ok(())
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

pub(super) async fn scheduler_honours_pod_affinity(context: &E2eContext) -> Result<()> {
    require_single_node(context).await?;
    let follower = "scheduler-affinity-follower";
    let anchor = "scheduler-affinity-anchor";
    create_pod(
        context,
        follower,
        json!({
            "affinity": {"podAffinity": {"requiredDuringSchedulingIgnoredDuringExecution": [{
                "labelSelector": {"matchLabels": {"scheduler-test": "anchor"}},
                "topologyKey": "kubernetes.io/hostname"
            }]}},
            "containers": [{"name": "app", "image": "busybox:latest", "command": ["sleep", "30"]}]
        }),
    )
    .await?;
    tokio::time::sleep(Duration::from_secs(5)).await;
    anyhow::ensure!(
        !pod_is_scheduled(context, follower).await?,
        "a pod affinity rule with no matching pod was satisfied"
    );
    create_labeled_pod(
        context,
        anchor,
        json!({"scheduler-test": "anchor"}),
        json!({"containers": [{"name": "app", "image": "busybox:latest", "command": ["sleep", "30"]}]}),
    )
    .await?;
    context
        .wait_until("pod affinity anchor to be scheduled", Duration::from_secs(60), || {
            pod_is_scheduled(context, anchor)
        })
        .await?;
    context
        .wait_until("pod affinity follower to be scheduled", Duration::from_secs(60), || {
            pod_is_scheduled(context, follower)
        })
        .await
}

pub(super) async fn scheduler_honours_pod_anti_affinity(context: &E2eContext) -> Result<()> {
    require_single_node(context).await?;
    let first = "scheduler-anti-affinity-first";
    let second = "scheduler-anti-affinity-second";
    create_labeled_pod(
        context,
        first,
        json!({"scheduler-test": "anti"}),
        json!({"containers": [{"name": "app", "image": "busybox:latest", "command": ["sleep", "30"]}]}),
    )
    .await?;
    context
        .wait_until("pod anti-affinity first Pod", Duration::from_secs(60), || {
            pod_is_scheduled(context, first)
        })
        .await?;
    create_labeled_pod(
        context,
        second,
        json!({"scheduler-test": "anti"}),
        json!({
            "affinity": {"podAntiAffinity": {"requiredDuringSchedulingIgnoredDuringExecution": [{
                "labelSelector": {"matchLabels": {"scheduler-test": "anti"}},
                "topologyKey": "kubernetes.io/hostname"
            }]}},
            "containers": [{"name": "app", "image": "busybox:latest", "command": ["sleep", "30"]}]
        }),
    )
    .await?;
    tokio::time::sleep(Duration::from_secs(5)).await;
    anyhow::ensure!(
        !pod_is_scheduled(context, second).await?,
        "pod anti-affinity allowed a second matching Pod onto the same node"
    );
    let pods: Api<Pod> = Api::namespaced(context.client.clone(), &context.namespace);
    pods.delete(first, &DeleteParams::default()).await?;
    context
        .wait_until("pod anti-affinity second Pod to be scheduled", Duration::from_secs(60), || {
            pod_is_scheduled(context, second)
        })
        .await
}

pub(super) async fn scheduler_honours_topology_spread(context: &E2eContext) -> Result<()> {
    require_single_node(context).await?;
    let first = "scheduler-spread-first";
    let second = "scheduler-spread-second";
    let spec = json!({
        "topologySpreadConstraints": [{
            "maxSkew": 1,
            "minDomains": 2,
            "topologyKey": "kubernetes.io/hostname",
            "whenUnsatisfiable": "DoNotSchedule",
            "labelSelector": {"matchLabels": {"scheduler-test": "spread"}}
        }],
        "containers": [{"name": "app", "image": "busybox:latest", "command": ["sleep", "30"]}]
    });
    create_labeled_pod(
        context,
        first,
        json!({"scheduler-test": "spread"}),
        spec.clone(),
    )
    .await?;
    context
        .wait_until("first topology-spread Pod", Duration::from_secs(60), || {
            pod_is_scheduled(context, first)
        })
        .await?;
    create_labeled_pod(
        context,
        second,
        json!({"scheduler-test": "spread"}),
        spec,
    )
    .await?;
    tokio::time::sleep(Duration::from_secs(5)).await;
    anyhow::ensure!(
        !pod_is_scheduled(context, second).await?,
        "topology spread allowed skew 2 in the only eligible topology domain"
    );
    Ok(())
}

pub(super) async fn scheduler_respects_a_taint_and_its_toleration(
    context: &E2eContext,
) -> Result<()> {
    require_single_node(context).await?;
    let node = first_node(context).await?;
    let node_name = node
        .metadata
        .name
        .clone()
        .context("the Node has no name")?;
    let nodes: Api<Node> = Api::all(context.client.clone());
    let original_taints = serde_json::to_value(
        node.spec
            .as_ref()
            .and_then(|spec| spec.taints.clone()),
    )?;
    let mut taints = original_taints.clone().as_array().cloned().unwrap_or_default();
    taints.push(json!({
        "key": "example.com/sched-test",
        "value": "yes",
        "effect": "NoSchedule"
    }));
    nodes
        .patch(
            &node_name,
            &PatchParams::default(),
            &Patch::Merge(&json!({"spec": {"taints": taints}})),
        )
        .await?;

    let result = async {
        let blocked = "scheduler-taint-blocked";
        let tolerated = "scheduler-taint-tolerated";
        create_pod(
            context,
            blocked,
            json!({
                "containers": [{"name": "app", "image": "busybox:latest", "command": ["sleep", "30"]}]
            }),
        )
        .await?;
        create_pod(
            context,
            tolerated,
            json!({
                "tolerations": [{"key": "example.com/sched-test", "operator": "Equal", "value": "yes", "effect": "NoSchedule"}],
                "containers": [{"name": "app", "image": "busybox:latest", "command": ["sleep", "30"]}]
            }),
        )
        .await?;
        context
            .wait_until("tolerating Pod to be scheduled", Duration::from_secs(60), || {
                pod_is_scheduled(context, tolerated)
            })
            .await?;
        anyhow::ensure!(
            !pod_is_scheduled(context, blocked).await?,
            "a Pod without a toleration was scheduled onto the tainted node"
        );
        nodes
            .patch(
                &node_name,
                &PatchParams::default(),
                &Patch::Merge(&json!({"spec": {"taints": original_taints.clone()}})),
            )
            .await?;
        context
            .wait_until("untolerating Pod to be scheduled after untaint", Duration::from_secs(60), || {
                pod_is_scheduled(context, blocked)
            })
            .await
    }
    .await;

    let restore = nodes
        .patch(
            &node_name,
            &PatchParams::default(),
            &Patch::Merge(&json!({"spec": {"taints": original_taints}})),
        )
        .await;
    restore?;
    result
}
