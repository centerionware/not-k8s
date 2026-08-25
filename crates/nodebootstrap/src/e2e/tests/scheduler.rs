use super::context::E2eContext;
use super::skip_test;
use anyhow::{Context, Result};
use k8s_openapi::api::core::v1::{Node, Pod};
use k8s_openapi::api::scheduling::v1::PriorityClass;
use kube::api::{Api, DeleteParams, ListParams, Patch, PatchParams, PostParams};
use serde_json::{json, Map, Value};
use std::process::Command;
use std::time::{Duration, Instant};

fn nodescheduler_is_active() -> bool {
    Command::new("systemctl")
        .args(["is-active", "--quiet", "nodescheduler.service"])
        .status()
        .is_ok_and(|status| status.success())
        || Command::new("pgrep")
            .args(["-x", "nodescheduler"])
            .status()
            .is_ok_and(|status| status.success())
}

fn require_nodescheduler() -> Result<()> {
    if nodescheduler_is_active() {
        Ok(())
    } else {
        Err(skip_test(
            "nodescheduler is not active; bootstrap with the replacement scheduler to exercise its lease",
        ))
    }
}

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

pub(super) async fn scheduler_rejects_a_pod_that_does_not_fit(
    context: &E2eContext,
) -> Result<()> {
    require_nodescheduler()?;
    let name = "scheduler-too-large";
    create_pod(
        context,
        name,
        json!({
            "restartPolicy": "Never",
            "containers": [{"name": "app", "image": "busybox:latest", "command": ["sleep", "300"], "resources": {"requests": {"cpu": "10000"}}}]
        }),
    )
    .await?;
    let pods: Api<Pod> = Api::namespaced(context.client.clone(), &context.namespace);
    context
        .wait_until("oversized Pod to remain unbound", Duration::from_secs(45), || {
            let pods = pods.clone();
            async move { Ok(pods.get(name).await?.spec.and_then(|spec| spec.node_name).is_none()) }
        })
        .await?;
    let pod = pods.get(name).await?;
    let pod_value = serde_json::to_value(pod)?;
    let message = pod_value
        .pointer("/status/conditions")
        .and_then(Value::as_array)
        .and_then(|conditions| {
            conditions.iter().find_map(|condition| {
                condition
                    .get("message")
                    .and_then(Value::as_str)
                    .filter(|message| message.contains("Insufficient cpu"))
            })
        });
    anyhow::ensure!(
        message.is_some(),
        "an oversized Pod stayed unbound but did not report an Insufficient cpu scheduling reason"
    );
    Ok(())
}

fn priority_class(name: &str, value: i32, preemption_policy: Option<&str>) -> PriorityClass {
    let mut class = json!({
        "apiVersion": "scheduling.k8s.io/v1",
        "kind": "PriorityClass",
        "metadata": {"name": name},
        "value": value,
        "globalDefault": false,
        "description": "nodebootstrap e2e priority class"
    });
    if let Some(policy) = preemption_policy {
        class["preemptionPolicy"] = json!(policy);
    }
    serde_json::from_value(class).expect("PriorityClass test fixture is valid")
}

async fn priority_scenario(
    context: &E2eContext,
    low_name: &str,
    high_name: &str,
    high_priority_class: &str,
    high_preemption_policy: Option<&str>,
) -> Result<()> {
    require_nodescheduler()?;
    require_single_node(context).await?;
    let node = first_node(context).await?;
    let allocatable = serde_json::to_value(node)?
        .pointer("/status/allocatable/cpu")
        .and_then(Value::as_str)
        .and_then(allocatable_cpu_millicores)
        .context("the Node has no usable allocatable CPU")?;
    let request = allocatable * 60 / 100;
    anyhow::ensure!(request > 0, "the Node has no CPU available for priority tests");
    let classes: Api<PriorityClass> = Api::all(context.client.clone());
    let low_class = priority_class("nodebootstrap-e2e-low", 100, None);
    let high_class = priority_class(
        high_priority_class,
        100_000,
        high_preemption_policy,
    );
    classes.create(&PostParams::default(), &low_class).await?;
    classes.create(&PostParams::default(), &high_class).await?;
    let pods: Api<Pod> = Api::namespaced(context.client.clone(), &context.namespace);
    let result = async {
        create_pod(
            context,
            low_name,
            json!({"priorityClassName": "nodebootstrap-e2e-low", "containers": [{"name": "app", "image": "busybox:latest", "command": ["sleep", "300"], "resources": {"requests": {"cpu": format!("{request}m")}}}]}),
        )
        .await?;
        context
            .wait_until("low-priority Pod to be scheduled", Duration::from_secs(90), || {
                pod_is_scheduled(context, low_name)
            })
            .await?;
        create_pod(
            context,
            high_name,
            json!({"priorityClassName": high_priority_class, "containers": [{"name": "app", "image": "busybox:latest", "command": ["sleep", "300"], "resources": {"requests": {"cpu": format!("{request}m")}}}]}),
        )
        .await?;
        Ok(())
    }
    .await;
    let result = match result {
        Ok(()) => {
            if high_preemption_policy == Some("Never") {
                tokio::time::sleep(Duration::from_secs(20)).await;
                anyhow::ensure!(
                    !pod_is_scheduled(context, high_name).await?,
                    "a high-priority Pod with preemptionPolicy=Never was scheduled despite insufficient capacity"
                );
                anyhow::ensure!(
                    pods.get_opt(low_name).await?.is_some(),
                    "preemptionPolicy=Never unexpectedly removed the lower-priority Pod"
                );
                Ok(())
            } else {
                context
                    .wait_until("high-priority Pod to preempt the low-priority Pod", Duration::from_secs(120), || {
                        pod_is_scheduled(context, high_name)
                    })
                    .await?;
                context
                    .wait_until("preempted low-priority Pod to disappear", Duration::from_secs(90), || {
                        let pods = pods.clone();
                        async move { Ok(pods.get_opt(low_name).await?.is_none()) }
                    })
                    .await
            }
        }
        Err(error) => Err(error),
    };
    let _ = pods.delete(low_name, &DeleteParams::default()).await;
    let _ = pods.delete(high_name, &DeleteParams::default()).await;
    let _ = classes.delete("nodebootstrap-e2e-low", &DeleteParams::default()).await;
    let _ = classes.delete(high_priority_class, &DeleteParams::default()).await;
    result
}

pub(super) async fn scheduler_preempts_a_lower_priority_pod(
    context: &E2eContext,
) -> Result<()> {
    priority_scenario(
        context,
        "scheduler-preempt-low",
        "scheduler-preempt-high",
        "nodebootstrap-e2e-high",
        None,
    )
    .await
}

pub(super) async fn scheduler_does_not_preempt_when_policy_forbids_it(
    context: &E2eContext,
) -> Result<()> {
    priority_scenario(
        context,
        "scheduler-no-preempt-low",
        "scheduler-no-preempt-high",
        "nodebootstrap-e2e-never",
        Some("Never"),
    )
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

pub(super) async fn scheduler_holds_the_leader_lease(context: &E2eContext) -> Result<()> {
    require_nodescheduler()?;
    let leases: Api<k8s_openapi::api::coordination::v1::Lease> =
        Api::namespaced(context.client.clone(), "kube-system");
    let lease = leases.get("kube-scheduler").await?;
    let value = serde_json::to_value(lease)?;
    let holder = value
        .pointer("/spec/holderIdentity")
        .and_then(Value::as_str)
        .filter(|holder| !holder.is_empty());
    anyhow::ensure!(
        holder.is_some(),
        "nodescheduler must hold the kube-scheduler lease in kube-system"
    );
    let first = value
        .pointer("/spec/renewTime")
        .and_then(Value::as_str)
        .filter(|renew_time| !renew_time.is_empty())
        .context("the kube-scheduler lease has no renewTime")?
        .to_string();
    context
        .wait_until("nodescheduler to renew the kube-scheduler lease", Duration::from_secs(45), || {
            let leases = leases.clone();
            let first = first.clone();
            async move {
                let lease = serde_json::to_value(leases.get("kube-scheduler").await?)?;
                Ok(lease
                    .pointer("/spec/renewTime")
                    .and_then(Value::as_str)
                    .is_some_and(|renew_time| !renew_time.is_empty() && renew_time != first))
            }
        })
        .await
}

fn allocatable_cpu_millicores(value: &str) -> Option<u64> {
    if let Some(value) = value.strip_suffix('m') {
        return value.parse().ok();
    }
    if let Some(value) = value.strip_suffix('n') {
        return value.parse::<u64>().ok().map(|nanos| nanos / 1_000_000);
    }
    value
        .parse::<u64>()
        .ok()
        .map(|cores| cores.saturating_mul(1_000))
}

pub(super) async fn scheduler_wakes_a_pending_pod_on_a_real_event(
    context: &E2eContext,
) -> Result<()> {
    require_nodescheduler()?;
    require_single_node(context).await?;
    let node = first_node(context).await?;
    let node_value = serde_json::to_value(node)?;
    let allocatable = node_value
        .pointer("/status/allocatable/cpu")
        .and_then(Value::as_str)
        .context("the Node has no allocatable CPU quantity")?;
    let each = allocatable_cpu_millicores(allocatable)
        .map(|milli| milli * 60 / 100)
        .filter(|milli| *milli > 0)
        .context("the Node reports no usable allocatable CPU")?;
    let pods: Api<Pod> = Api::namespaced(context.client.clone(), &context.namespace);
    let blocker = "scheduler-event-blocker";
    let waiter = "scheduler-event-waiter";
    let pod_spec = |cpu: u64| {
        json!({
            "containers": [{
                "name": "app",
                "image": "busybox:latest",
                "command": ["sleep", "300"],
                "resources": {"requests": {"cpu": format!("{cpu}m")}}
            }]
        })
    };
    let result = async {
        create_pod(context, blocker, pod_spec(each)).await?;
        context
            .wait_until("scheduler blocker to be bound", Duration::from_secs(60), || {
                pod_is_scheduled(context, blocker)
            })
            .await?;
        create_pod(context, waiter, pod_spec(each)).await?;
        tokio::time::sleep(Duration::from_secs(8)).await;
        anyhow::ensure!(
            !pod_is_scheduled(context, waiter).await?,
            "the second 60%-CPU Pod was scheduled alongside the blocker"
        );

        pods.delete(blocker, &DeleteParams::default()).await?;
        context
            .wait_until("scheduler blocker to disappear", Duration::from_secs(120), || {
                let pods = pods.clone();
                async move { Ok(pods.get_opt(blocker).await?.is_none()) }
            })
            .await?;
        let freed = Instant::now();
        context
            .wait_until("waiting Pod to be scheduled after the blocker disappears", Duration::from_secs(120), || {
                pod_is_scheduled(context, waiter)
            })
            .await?;
        anyhow::ensure!(
            freed.elapsed() < Duration::from_secs(60),
            "the pending Pod was not scheduled promptly after the resource-freeing delete; the scheduler event hint may be missing"
        );
        Ok(())
    }
    .await;
    let _ = pods.delete(blocker, &DeleteParams::default()).await;
    let _ = pods.delete(waiter, &DeleteParams::default()).await;
    result
}
