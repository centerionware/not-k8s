use super::context::E2eContext;
use super::skip_test;
use anyhow::{Context, Result};
use k8s_openapi::api::apps::v1::Deployment;
use k8s_openapi::api::core::v1::{Event, Namespace, Node, PersistentVolume, PersistentVolumeClaim, Pod, Service};
use k8s_openapi::api::storage::v1::StorageClass;
use k8s_openapi::api::scheduling::v1::PriorityClass;
use kube::api::{Api, DeleteParams, ListParams, Patch, PatchParams, PostParams};
use serde_json::{json, Map, Value};
use std::io::Write;
use std::process::{Command, Stdio};
use std::sync::{atomic::{AtomicUsize, Ordering}, Arc};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

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

struct FakeExtender {
    port: u16,
    calls: Arc<AtomicUsize>,
    task: tokio::task::JoinHandle<()>,
}

async fn fake_extender_connection(
    mut stream: tokio::net::TcpStream,
    reject: bool,
    calls: Arc<AtomicUsize>,
) -> Result<()> {
    let mut request = Vec::new();
    let mut buffer = [0_u8; 8192];
    let (header_end, content_length) = loop {
        let read = stream.read(&mut buffer).await?;
        if read == 0 {
            anyhow::bail!("fake scheduler extender received an incomplete HTTP request");
        }
        request.extend_from_slice(&buffer[..read]);
        let Some(header_end) = request.windows(4).position(|window| window == b"\r\n\r\n") else {
            continue;
        };
        let headers = std::str::from_utf8(&request[..header_end])?;
        let content_length = headers
            .lines()
            .find_map(|line| line.strip_prefix("Content-Length:").or_else(|| line.strip_prefix("content-length:")))
            .and_then(|value| value.trim().parse::<usize>().ok())
            .unwrap_or_default();
        if request.len() >= header_end + 4 + content_length {
            break (header_end, content_length);
        }
    };
    calls.fetch_add(1, Ordering::Relaxed);
    let body_start = header_end + 4;
    let request_body: Value = serde_json::from_slice(&request[body_start..body_start + content_length])?;
    let node_names = request_body
        .get("NodeNames")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut response = json!({"NodeNames": node_names});
    if reject {
        let failed = response["NodeNames"]
            .as_array()
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .filter_map(|name| name.as_str().map(|name| (name.to_owned(), "no-gpu-quota-fake-extender")))
            .collect::<Vec<_>>();
        response["NodeNames"] = json!([]);
        response["FailedNodes"] = Value::Object(
            failed
                .into_iter()
                .map(|(name, reason)| (name, Value::String(reason.to_owned())))
                .collect(),
        );
    }
    let response_body = serde_json::to_vec(&response)?;
    let response_head = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        response_body.len()
    );
    stream.write_all(response_head.as_bytes()).await?;
    stream.write_all(&response_body).await?;
    Ok(())
}

async fn start_fake_extender(reject: bool) -> Result<FakeExtender> {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
    let port = listener.local_addr()?.port();
    let calls = Arc::new(AtomicUsize::new(0));
    let calls_for_task = calls.clone();
    let task = tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else { break };
            let calls = calls_for_task.clone();
            tokio::spawn(async move {
                let _ = fake_extender_connection(stream, reject, calls).await;
            });
        }
    });
    Ok(FakeExtender { port, calls, task })
}

fn scheduler_override_path() -> std::path::PathBuf {
    "/etc/systemd/system/nodescheduler.service.d/99-nodebootstrap-e2e.conf".into()
}

fn run_systemctl(args: &[&str]) -> Result<()> {
    let uid = Command::new("id").arg("-u").output()?;
    let uid = String::from_utf8_lossy(&uid.stdout).trim().to_owned();
    let mut command = if uid == "0" {
        let mut command = Command::new("systemctl");
        command.args(args);
        command
    } else {
        let mut command = Command::new("sudo");
        command.arg("systemctl").args(args);
        command
    };
    let output = command.output()?;
    anyhow::ensure!(
        output.status.success(),
        "systemctl {} failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(())
}

fn write_scheduler_override(contents: Option<&str>) -> Result<()> {
    let path = scheduler_override_path();
    let uid = Command::new("id").arg("-u").output()?;
    let uid = String::from_utf8_lossy(&uid.stdout).trim().to_owned();
    if uid == "0" {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        match contents {
            Some(contents) => std::fs::write(&path, contents)?,
            None => {
                let _ = std::fs::remove_file(&path);
            }
        }
    } else if let Some(contents) = contents {
        let parent = path
            .parent()
            .context("scheduler override path has no parent")?
            .to_str()
            .context("scheduler override parent is not UTF-8")?;
        let mkdir = Command::new("sudo").args(["mkdir", "-p", parent]).status()?;
        anyhow::ensure!(mkdir.success(), "sudo mkdir failed creating scheduler override directory");
        let mut child = Command::new("sudo")
            .args(["tee", path.to_str().context("scheduler override path is not UTF-8")?])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .spawn()?;
        child
            .stdin
            .take()
            .context("sudo tee did not provide stdin")?
            .write_all(contents.as_bytes())?;
        anyhow::ensure!(child.wait()?.success(), "sudo tee failed writing scheduler override");
    } else {
        let status = Command::new("sudo")
            .args(["rm", "-f", path.to_str().context("scheduler override path is not UTF-8")?])
            .status()?;
        anyhow::ensure!(status.success(), "sudo rm failed removing scheduler override");
    }
    Ok(())
}

async fn scheduler_lease_renew_time(context: &E2eContext) -> Result<String> {
    let leases: Api<k8s_openapi::api::coordination::v1::Lease> =
        Api::namespaced(context.client.clone(), "kube-system");
    Ok(serde_json::to_value(leases.get("kube-scheduler").await?)?
        .pointer("/spec/renewTime")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned())
}

async fn restart_scheduler_with_env(context: &E2eContext, contents: Option<&str>) -> Result<()> {
    let before = scheduler_lease_renew_time(context).await?;
    write_scheduler_override(contents)?;
    let restarted = run_systemctl(&["daemon-reload"])
        .and_then(|_| run_systemctl(&["restart", "nodescheduler.service"]));
    if let Err(error) = restarted {
        let _ = write_scheduler_override(None);
        let _ = run_systemctl(&["daemon-reload"]);
        return Err(error);
    }
    context
        .wait_until("nodescheduler to reacquire its leader lease", Duration::from_secs(90), || {
            let before = before.clone();
            async move {
                let now = scheduler_lease_renew_time(context).await?;
                Ok(!now.is_empty() && now != before)
            }
        })
        .await
}

async fn scheduler_extender_case(context: &E2eContext, reject: bool) -> Result<()> {
    require_nodescheduler()?;
    if !Command::new("systemctl")
        .args(["cat", "nodescheduler.service"])
        .status()
        .is_ok_and(|status| status.success())
    {
        return Err(skip_test(
            "HTTP extender tests require a systemd-managed nodescheduler service",
        ));
    }
    let extender = start_fake_extender(reject).await?;
    let config = serde_json::to_string(&json!([{
        "urlPrefix": format!("http://127.0.0.1:{}", extender.port),
        "filterVerb": "filter",
        "nodeCacheCapable": true
    }]))?;
    let escaped = config.replace('"', "\\\"");
    let override_contents = format!(
        "[Service]\nEnvironment=\"NODESCHEDULER_EXTENDERS_JSON={escaped}\"\n"
    );
    let setup = restart_scheduler_with_env(context, Some(&override_contents)).await;
    let result = async {
        setup?;
        let name = if reject { "sched-extender-reject" } else { "sched-extender-accept" };
        let pods: Api<Pod> = Api::namespaced(context.client.clone(), &context.namespace);
        create_pod(
            context,
            name,
            json!({"containers": [{"name": "app", "image": "busybox:latest", "command": ["sleep", "300"]}]}),
        )
        .await?;
        if reject {
            context
                .wait_until("extender-rejected Pod to report its scheduling event", Duration::from_secs(60), || {
                    let events: Api<Event> = Api::namespaced(context.client.clone(), &context.namespace);
                    async move {
                        Ok(events.list(&ListParams::default()).await?.items.into_iter().any(|event| {
                            let value = serde_json::to_value(event).unwrap_or_default();
                            value.pointer("/involvedObject/name").and_then(Value::as_str) == Some(name)
                                && value.pointer("/message").and_then(Value::as_str).is_some_and(|message| message.contains("no-gpu-quota-fake-extender"))
                        }))
                    }
                })
                .await?;
            anyhow::ensure!(!pod_is_scheduled(context, name).await?, "the HTTP extender rejected Pod {name}, but it was scheduled");
        } else {
            context
                .wait_until("extender-approved Pod to be scheduled", Duration::from_secs(90), || {
                    pod_is_scheduled(context, name)
                })
                .await?;
        }
        anyhow::ensure!(extender.calls.load(Ordering::Relaxed) > 0, "nodescheduler never called the HTTP extender");
        let _ = pods.delete(name, &DeleteParams::default()).await;
        Ok(())
    }
    .await;
    let _ = restart_scheduler_with_env(context, None).await;
    extender.task.abort();
    result
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

pub(super) async fn scheduler_consults_an_http_extender_and_honours_a_filter_rejection(
    context: &E2eContext,
) -> Result<()> {
    scheduler_extender_case(context, true).await
}

pub(super) async fn scheduler_schedules_a_pod_an_http_extender_approves(
    context: &E2eContext,
) -> Result<()> {
    scheduler_extender_case(context, false).await
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
    let events: Api<Event> = Api::namespaced(context.client.clone(), &context.namespace);
    context
        .wait_until(
            "oversized Pod to remain unbound with an Insufficient cpu scheduling event",
            Duration::from_secs(60),
            || {
                let pods = pods.clone();
                let events = events.clone();
                async move {
                    let pod = pods.get(name).await?;
                    if pod.spec.and_then(|spec| spec.node_name).is_some() {
                        return Ok(false);
                    }
                    Ok(events.list(&ListParams::default()).await?.items.iter().any(|event| {
                        event.involved_object.name.as_deref() == Some(name)
                            && event.reason.as_deref() == Some("FailedScheduling")
                            && event
                                .message
                                .as_deref()
                                .is_some_and(|message| message.contains("Insufficient cpu"))
                    }))
                }
            },
        )
        .await?;
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

pub(super) async fn scheduler_resolves_a_namespace_selector_against_real_labels(
    context: &E2eContext,
) -> Result<()> {
    require_nodescheduler()?;
    require_single_node(context).await?;
    let helper_namespace = "nodebootstrap-e2e-selector-namespace";
    let namespaces: Api<Namespace> = Api::all(context.client.clone());
    let _ = namespaces.delete(helper_namespace, &DeleteParams::default()).await;
    let namespace: Namespace = serde_json::from_value(json!({
        "apiVersion": "v1",
        "kind": "Namespace",
        "metadata": {"name": helper_namespace, "labels": {"nodebootstrap-e2e-selector": "other"}}
    }))?;
    namespaces.create(&PostParams::default(), &namespace).await?;
    let other_pods: Api<Pod> = Api::namespaced(context.client.clone(), helper_namespace);
    let blocker: Pod = serde_json::from_value(json!({
        "apiVersion": "v1",
        "kind": "Pod",
        "metadata": {"name": "namespace-selector-blocker", "labels": {"nodebootstrap-e2e-selector": "match"}},
        "spec": {"containers": [{"name": "app", "image": "busybox:latest", "command": ["sleep", "300"]}]}
    }))?;
    other_pods.create(&PostParams::default(), &blocker).await?;
    context
        .wait_until("namespaceSelector blocker to be scheduled", Duration::from_secs(90), || {
            let other_pods = other_pods.clone();
            async move { Ok(other_pods.get("namespace-selector-blocker").await?.spec.and_then(|spec| spec.node_name).is_some()) }
        })
        .await?;

    let pods: Api<Pod> = Api::namespaced(context.client.clone(), &context.namespace);
    let blocked = "scheduler-namespace-selector-blocked";
    create_pod(
        context,
        blocked,
        json!({
            "affinity": {"podAntiAffinity": {"requiredDuringSchedulingIgnoredDuringExecution": [{
                "labelSelector": {"matchLabels": {"nodebootstrap-e2e-selector": "match"}},
                "namespaceSelector": {"matchLabels": {"nodebootstrap-e2e-selector": "other"}},
                "topologyKey": "kubernetes.io/hostname"
            }]}},
            "containers": [{"name": "app", "image": "busybox:latest", "command": ["sleep", "300"]}]
        }),
    )
    .await?;
    tokio::time::sleep(Duration::from_secs(5)).await;
    anyhow::ensure!(!pod_is_scheduled(context, blocked).await?, "namespaceSelector failed to apply the matching namespace");

    let allowed = "scheduler-namespace-selector-allowed";
    create_pod(
        context,
        allowed,
        json!({
            "affinity": {"podAntiAffinity": {"requiredDuringSchedulingIgnoredDuringExecution": [{
                "labelSelector": {"matchLabels": {"nodebootstrap-e2e-selector": "match"}},
                "namespaceSelector": {"matchLabels": {"nodebootstrap-e2e-selector": "does-not-exist"}},
                "topologyKey": "kubernetes.io/hostname"
            }]}},
            "containers": [{"name": "app", "image": "busybox:latest", "command": ["sleep", "300"]}]
        }),
    )
    .await?;
    let result = context
        .wait_until("namespaceSelector non-matching Pod to be scheduled", Duration::from_secs(60), || {
            pod_is_scheduled(context, allowed)
        })
        .await;
    let _ = pods.delete(blocked, &DeleteParams::default()).await;
    let _ = pods.delete(allowed, &DeleteParams::default()).await;
    let _ = namespaces.delete(helper_namespace, &DeleteParams::default()).await;
    result
}

pub(super) async fn scheduler_schedules_pods_that_get_default_spread_constraints(
    context: &E2eContext,
) -> Result<()> {
    require_nodescheduler()?;
    let name = "scheduler-default-spread";
    let deployments: Api<Deployment> = Api::namespaced(context.client.clone(), &context.namespace);
    let services: Api<Service> = Api::namespaced(context.client.clone(), &context.namespace);
    let pods: Api<Pod> = Api::namespaced(context.client.clone(), &context.namespace);
    let service: Service = serde_json::from_value(json!({
        "apiVersion": "v1", "kind": "Service", "metadata": {"name": name},
        "spec": {"selector": {"app": name}, "ports": [{"port": 80}]}
    }))?;
    services.create(&PostParams::default(), &service).await?;
    let deployment: Deployment = serde_json::from_value(json!({
        "apiVersion": "apps/v1", "kind": "Deployment", "metadata": {"name": name},
        "spec": {"replicas": 2, "selector": {"matchLabels": {"app": name}}, "template": {
            "metadata": {"labels": {"app": name, "tier": "front"}},
            "spec": {"containers": [{"name": "app", "image": "busybox:latest", "command": ["sleep", "300"]}]}
        }}
    }))?;
    deployments.create(&PostParams::default(), &deployment).await?;
    let result = context
        .wait_until("default-spread Deployment Pods to be scheduled", Duration::from_secs(120), || {
            let pods = pods.clone();
            async move {
                let listed = pods.list(&ListParams::default().labels(&format!("app={name}"))).await?;
                Ok(listed.items.len() == 2 && listed.items.iter().all(|pod| pod.spec.as_ref().and_then(|spec| spec.node_name.as_ref()).is_some()))
            }
        })
        .await;
    let _ = deployments.delete(name, &DeleteParams::default()).await;
    let _ = services.delete(name, &DeleteParams::default()).await;
    result
}

pub(super) async fn scheduler_delays_binding_a_wait_for_first_consumer_pvc_until_a_node_is_chosen(
    context: &E2eContext,
) -> Result<()> {
    require_nodescheduler()?;
    if crate::config::Config::from_env()?.nodelet_runtime() != "cri" {
        return Err(skip_test("WaitForFirstConsumer checks require the CRI runtime"));
    }
    let class_name = std::env::var("TEST_CSI_STORAGE_CLASS_WAIT")
        .ok()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            skip_test(
                "TEST_CSI_STORAGE_CLASS_WAIT is not set; an external WaitForFirstConsumer provisioner is required",
            )
        })?;
    let classes: Api<StorageClass> = Api::all(context.client.clone());
    if classes.get(&class_name).await.is_err() {
        return Err(skip_test(format!(
            "WaitForFirstConsumer StorageClass {class_name} is not installed"
        )));
    }

    let claim_name = "scheduler-wfc-claim";
    let pod_name = "scheduler-wfc-pod";
    let pvcs: Api<PersistentVolumeClaim> =
        Api::namespaced(context.client.clone(), &context.namespace);
    let pods: Api<Pod> = Api::namespaced(context.client.clone(), &context.namespace);
    let pvc: PersistentVolumeClaim = serde_json::from_value(json!({
        "apiVersion": "v1",
        "kind": "PersistentVolumeClaim",
        "metadata": {"name": claim_name},
        "spec": {
            "accessModes": ["ReadWriteOnce"],
            "storageClassName": class_name,
            "resources": {"requests": {"storage": "64Mi"}}
        }
    }))?;
    pvcs.create(&PostParams::default(), &pvc).await?;

    let result = async {
        tokio::time::sleep(Duration::from_secs(8)).await;
        anyhow::ensure!(
            pvcs.get(claim_name)
                .await?
                .status
                .and_then(|status| status.phase)
                .as_deref()
                == Some("Pending"),
            "WaitForFirstConsumer PVC bound before a Pod selected a node"
        );

        let pod: Pod = serde_json::from_value(json!({
            "apiVersion": "v1",
            "kind": "Pod",
            "metadata": {"name": pod_name},
            "spec": {
                "containers": [{
                    "name": "app",
                    "image": "busybox:latest",
                    "command": ["sleep", "300"],
                    "volumeMounts": [{"name": "data", "mountPath": "/data"}]
                }],
                "volumes": [{"name": "data", "persistentVolumeClaim": {"claimName": claim_name}}]
            }
        }))?;
        pods.create(&PostParams::default(), &pod).await?;

        if let Err(error) = context
            .wait_until("WaitForFirstConsumer Pod to reach Running", Duration::from_secs(90), || {
                let pods = pods.clone();
                async move {
                    Ok(pods
                        .get(pod_name)
                        .await?
                        .status
                        .and_then(|status| status.phase)
                        .as_deref()
                        == Some("Running"))
                }
            })
            .await
        {
            let phase = pvcs
                .get(claim_name)
                .await
                .ok()
                .and_then(|claim| claim.status)
                .and_then(|status| status.phase)
                .unwrap_or_else(|| "unknown".to_owned());
            return Err(skip_test(format!(
                "PVC never bound after a node was chosen (phase={phase}): {error}; an external provisioner is required"
            )));
        }

        let node = pods
            .get(pod_name)
            .await?
            .spec
            .and_then(|spec| spec.node_name)
            .context("scheduled Pod has no nodeName")?;
        let claim = pvcs.get(claim_name).await?;
        let selected_node = claim
            .metadata
            .annotations
            .as_ref()
            .and_then(|annotations| annotations.get("volume.kubernetes.io/selected-node"))
            .context("WaitForFirstConsumer PVC has no selected-node annotation")?;
        anyhow::ensure!(
            selected_node == &node,
            "VolumeBinding selected node {selected_node:?}, but the Pod was scheduled to {node}"
        );
        anyhow::ensure!(
            claim
                .status
                .and_then(|status| status.phase)
                .as_deref()
                == Some("Bound"),
            "PVC was not Bound after its Pod reached Running"
        );
        Ok(())
    }
    .await;
    let _ = pods.delete(pod_name, &DeleteParams::default()).await;
    let _ = pvcs.delete(claim_name, &DeleteParams::default()).await;
    result
}

pub(super) async fn scheduler_claims_a_static_wait_for_first_consumer_volume(
    context: &E2eContext,
) -> Result<()> {
    require_nodescheduler()?;
    let class_name = "nodebootstrap-e2e-static-wfc";
    let pv_name = "nodebootstrap-e2e-static-wfc-pv";
    let pvc_name = "nodebootstrap-e2e-static-wfc-pvc";
    let pod_name = "nodebootstrap-e2e-static-wfc-pod";
    let classes: Api<StorageClass> = Api::all(context.client.clone());
    let pvs: Api<PersistentVolume> = Api::all(context.client.clone());
    let pvcs: Api<PersistentVolumeClaim> = Api::namespaced(context.client.clone(), &context.namespace);
    let pods: Api<Pod> = Api::namespaced(context.client.clone(), &context.namespace);
    let class: StorageClass = serde_json::from_value(json!({
        "apiVersion": "storage.k8s.io/v1", "kind": "StorageClass", "metadata": {"name": class_name},
        "provisioner": "kubernetes.io/no-provisioner", "volumeBindingMode": "WaitForFirstConsumer"
    }))?;
    classes.create(&PostParams::default(), &class).await?;
    let pv: PersistentVolume = serde_json::from_value(json!({
        "apiVersion": "v1", "kind": "PersistentVolume", "metadata": {"name": pv_name},
        "spec": {"capacity": {"storage": "64Mi"}, "accessModes": ["ReadWriteOnce"], "storageClassName": class_name, "persistentVolumeReclaimPolicy": "Retain", "hostPath": {"path": "/tmp/nodebootstrap-e2e-static-wfc", "type": "DirectoryOrCreate"}}
    }))?;
    pvs.create(&PostParams::default(), &pv).await?;
    let pvc: PersistentVolumeClaim = serde_json::from_value(json!({
        "apiVersion": "v1", "kind": "PersistentVolumeClaim", "metadata": {"name": pvc_name},
        "spec": {"accessModes": ["ReadWriteOnce"], "storageClassName": class_name, "resources": {"requests": {"storage": "32Mi"}}}
    }))?;
    pvcs.create(&PostParams::default(), &pvc).await?;
    let result = async {
        tokio::time::sleep(Duration::from_secs(5)).await;
        anyhow::ensure!(pvcs.get(pvc_name).await?.status.and_then(|status| status.phase).as_deref() == Some("Pending"), "WaitForFirstConsumer PVC bound before a Pod selected a node");
        let pod: Pod = serde_json::from_value(json!({
            "apiVersion": "v1", "kind": "Pod", "metadata": {"name": pod_name},
            "spec": {"containers": [{"name": "app", "image": "busybox:latest", "command": ["sleep", "300"], "volumeMounts": [{"name": "data", "mountPath": "/data"}]}], "volumes": [{"name": "data", "persistentVolumeClaim": {"claimName": pvc_name}}]}
        }))?;
        pods.create(&PostParams::default(), &pod).await?;
        context.wait_until("static WaitForFirstConsumer Pod to be scheduled", Duration::from_secs(90), || pod_is_scheduled(context, pod_name)).await?;
        context.wait_until("static WaitForFirstConsumer PVC to bind", Duration::from_secs(90), || {
            let pvcs = pvcs.clone();
            async move { Ok(pvcs.get(pvc_name).await?.status.and_then(|status| status.phase).as_deref() == Some("Bound")) }
        }).await?;
        let claim = pvcs.get(pvc_name).await?;
        anyhow::ensure!(claim.spec.and_then(|spec| spec.volume_name).as_deref() == Some(pv_name), "the binder did not publish the scheduler's static PV choice");
        Ok(())
    }.await;
    let _ = pods.delete(pod_name, &DeleteParams::default()).await;
    let _ = pvcs.delete(pvc_name, &DeleteParams::default()).await;
    let _ = pvs.delete(pv_name, &DeleteParams::default()).await;
    let _ = classes.delete(class_name, &DeleteParams::default()).await;
    result
}

/// Exercise the event order that a watch-driven scheduler and nodelet must
/// tolerate: the Pod arrives first, its PVC arrives second, and the matching
/// PV arrives last. Each intermediate assertion is deliberately made before
/// creating the next object, so a test that only succeeds through a later
/// relist or timeout cannot pass this case.
pub(super) async fn scheduler_retries_a_pod_through_late_pvc_and_pv_events(
    context: &E2eContext,
) -> Result<()> {
    require_nodescheduler()?;
    if crate::config::Config::from_env()?.nodelet_runtime() != "cri" {
        return Err(skip_test("late PVC/PV ordering checks require the CRI runtime"));
    }
    let suffix = std::process::id();
    let class_name = format!("late-wfc-{suffix}");
    let pvc_name = format!("late-pvc-{suffix}");
    let pv_name = format!("late-pv-{suffix}");
    let pod_name = format!("late-pod-{suffix}");
    let host_path = format!("/tmp/nodebootstrap-e2e-{pv_name}");
    let classes: Api<StorageClass> = Api::all(context.client.clone());
    let pvcs: Api<PersistentVolumeClaim> =
        Api::namespaced(context.client.clone(), &context.namespace);
    let pvs: Api<PersistentVolume> = Api::all(context.client.clone());
    let pods: Api<Pod> = Api::namespaced(context.client.clone(), &context.namespace);

    let class: StorageClass = serde_json::from_value(json!({
        "apiVersion": "storage.k8s.io/v1",
        "kind": "StorageClass",
        "metadata": {"name": class_name},
        "provisioner": "kubernetes.io/no-provisioner",
        "volumeBindingMode": "WaitForFirstConsumer"
    }))?;
    classes.create(&PostParams::default(), &class).await?;
    context
        .wait_until("late StorageClass to be visible", Duration::from_secs(30), || {
            let classes = classes.clone();
            let class_name = class_name.clone();
            async move { Ok(classes.get(&class_name).await.is_ok()) }
        })
        .await?;

    let pod: Pod = serde_json::from_value(json!({
        "apiVersion": "v1",
        "kind": "Pod",
        "metadata": {"name": pod_name},
        "spec": {
            "restartPolicy": "Never",
            "containers": [{
                "name": "app",
                "image": "busybox:latest",
                "command": ["sleep", "300"],
                "volumeMounts": [{"name": "data", "mountPath": "/data"}]
            }],
            "volumes": [{"name": "data", "persistentVolumeClaim": {"claimName": pvc_name}}]
        }
    }))?;
    pods.create(&PostParams::default(), &pod).await?;

    let pvc: PersistentVolumeClaim = serde_json::from_value(json!({
        "apiVersion": "v1",
        "kind": "PersistentVolumeClaim",
        "metadata": {"name": pvc_name},
        "spec": {
            "accessModes": ["ReadWriteOnce"],
            "storageClassName": class_name,
            "resources": {"requests": {"storage": "32Mi"}}
        }
    }))?;
    let pv: PersistentVolume = serde_json::from_value(json!({
        "apiVersion": "v1",
        "kind": "PersistentVolume",
        "metadata": {"name": pv_name},
        "spec": {
            "capacity": {"storage": "64Mi"},
            "accessModes": ["ReadWriteOnce"],
            "storageClassName": class_name,
            "persistentVolumeReclaimPolicy": "Retain",
            "hostPath": {"path": host_path, "type": "DirectoryOrCreate"}
        }
    }))?;

    let result = async {
        // The scheduler must reject the Pod, but leave it unscheduled, while
        // the referenced PVC is genuinely absent.
        context
            .wait_until("late PVC Pod to report its missing claim", Duration::from_secs(60), || {
                let pods = pods.clone();
                let pod_name = pod_name.clone();
                let pvc_name = pvc_name.clone();
                async move {
                    let pod = pods.get(&pod_name).await?;
                    Ok(pod.spec.as_ref().and_then(|spec| spec.node_name.as_ref()).is_none()
                        && pod.status.as_ref().and_then(|status| status.phase.as_deref()) == Some("Pending")
                        && pod.status.as_ref().and_then(|status| status.conditions.as_ref()).is_some_and(|conditions| {
                            conditions.iter().any(|condition| {
                                condition.type_ == "PodScheduled"
                                    && condition.status == "False"
                                    && condition.reason.as_deref() == Some("Unschedulable")
                                    && condition.message.as_deref().is_some_and(|message| message.contains(&pvc_name))
                            })
                        }))
                }
            })
            .await?;

        // Deliver only the PVC. It is still impossible to start: no PV exists
        // yet, and this assertion catches a scheduler that loses the parked
        // Pod when the PVC ADD event updates its cache.
        pvcs.create(&PostParams::default(), &pvc).await?;
        context
            .wait_until("late PVC Pod to remain Pending before its PV exists", Duration::from_secs(60), || {
                let pods = pods.clone();
                let pvcs = pvcs.clone();
                let pod_name = pod_name.clone();
                let pvc_name = pvc_name.clone();
                async move {
                    let pod = pods.get(&pod_name).await?;
                    let claim = pvcs.get(&pvc_name).await?;
                    Ok(pod.spec.as_ref().and_then(|spec| spec.node_name.as_ref()).is_none()
                        && pod.status.as_ref().and_then(|status| status.phase.as_deref()) == Some("Pending")
                        && claim.status.as_ref().and_then(|status| status.phase.as_deref()) != Some("Bound"))
                }
            })
            .await?;

        // Only now make the PV available. The PV ADD event must wake the same
        // parked scheduling attempt, the binder must complete the claim, and
        // nodelet must then reconcile the already-existing Pod to Running.
        pvs.create(&PostParams::default(), &pv).await?;
        context
            .wait_until("late PVC to bind after its PV appears", Duration::from_secs(90), || {
                let pvcs = pvcs.clone();
                let pvc_name = pvc_name.clone();
                async move {
                    Ok(pvcs
                        .get(&pvc_name)
                        .await?
                        .status
                        .and_then(|status| status.phase)
                        .as_deref()
                        == Some("Bound"))
                }
            })
            .await?;
        context
            .wait_until("late PVC Pod to be scheduled after its PV binds", Duration::from_secs(90), || {
                pod_is_scheduled(context, &pod_name)
            })
            .await?;
        context
            .wait_until("late PVC Pod to reach Running", Duration::from_secs(120), || {
                let pods = pods.clone();
                let pod_name = pod_name.clone();
                async move {
                    Ok(pods
                        .get(&pod_name)
                        .await?
                        .status
                        .and_then(|status| status.phase)
                        .as_deref()
                        == Some("Running"))
                }
            })
            .await?;
        Ok::<(), anyhow::Error>(())
    }
    .await;
    let _ = pods.delete(&pod_name, &DeleteParams::default()).await;
    let _ = pvcs.delete(&pvc_name, &DeleteParams::default()).await;
    let _ = pvs.delete(&pv_name, &DeleteParams::default()).await;
    let _ = classes.delete(&class_name, &DeleteParams::default()).await;
    let _ = std::fs::remove_dir_all(&host_path);
    result
}

pub(super) async fn scheduler_enforces_read_write_once_pod_exclusivity(
    context: &E2eContext,
) -> Result<()> {
    require_nodescheduler()?;
    let Some(class_name) = std::env::var("TEST_CSI_STORAGE_CLASS").ok().filter(|value| !value.is_empty()) else {
        return Err(skip_test("TEST_CSI_STORAGE_CLASS is not set for the ReadWriteOncePod scheduler case"));
    };
    if crate::config::Config::from_env()?.nodelet_runtime() != "cri" {
        return Err(skip_test("ReadWriteOncePod scheduling requires the CRI runtime"));
    }
    let pvc_name = "nodebootstrap-e2e-rwop-pvc";
    let first = "nodebootstrap-e2e-rwop-first";
    let second = "nodebootstrap-e2e-rwop-second";
    let pvcs: Api<PersistentVolumeClaim> = Api::namespaced(context.client.clone(), &context.namespace);
    let pods: Api<Pod> = Api::namespaced(context.client.clone(), &context.namespace);
    let claim: PersistentVolumeClaim = serde_json::from_value(json!({
        "apiVersion": "v1", "kind": "PersistentVolumeClaim", "metadata": {"name": pvc_name},
        "spec": {"accessModes": ["ReadWriteOncePod"], "storageClassName": class_name, "resources": {"requests": {"storage": "64Mi"}}}
    }))?;
    pvcs.create(&PostParams::default(), &claim).await?;
    let result = async {
        context.wait_until("ReadWriteOncePod claim to bind", Duration::from_secs(120), || {
            let pvcs = pvcs.clone();
            async move { Ok(pvcs.get(pvc_name).await?.status.and_then(|status| status.phase).as_deref() == Some("Bound")) }
        }).await?;
        for name in [first, second] {
            let pod: Pod = serde_json::from_value(json!({
                "apiVersion": "v1", "kind": "Pod", "metadata": {"name": name},
                "spec": {"containers": [{"name": "app", "image": "busybox:latest", "command": ["sleep", "300"], "volumeMounts": [{"name": "data", "mountPath": "/data"}]}], "volumes": [{"name": "data", "persistentVolumeClaim": {"claimName": pvc_name}}]}
            }))?;
            pods.create(&PostParams::default(), &pod).await?;
            if name == first {
                context.wait_until("first ReadWriteOncePod to be scheduled", Duration::from_secs(90), || pod_is_scheduled(context, first)).await?;
            }
        }
        tokio::time::sleep(Duration::from_secs(10)).await;
        anyhow::ensure!(!pod_is_scheduled(context, second).await?, "second ReadWriteOncePod consumer was scheduled while the first held the claim");
        pods.delete(first, &DeleteParams::default()).await?;
        context.wait_until("first ReadWriteOncePod to disappear", Duration::from_secs(120), || {
            let pods = pods.clone();
            async move { Ok(pods.get_opt(first).await?.is_none()) }
        }).await?;
        context.wait_until("second ReadWriteOncePod to be scheduled after release", Duration::from_secs(90), || pod_is_scheduled(context, second)).await
    }.await;
    let _ = pods.delete(first, &DeleteParams::default()).await;
    let _ = pods.delete(second, &DeleteParams::default()).await;
    let _ = pvcs.delete(pvc_name, &DeleteParams::default()).await;
    result
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
