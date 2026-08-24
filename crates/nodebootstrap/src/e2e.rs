//! Bootstrap-native end-to-end checks.
//!
//! This is the 0.7.1 migration seam from the shell e2e suite: checks live in
//! the bootstrap applet, use the Kubernetes API directly, and do not assume
//! k3s-specific flags, paths, services, or command wrappers. Each migrated
//! shell case becomes another entry here, with the metadata used
//! by CI to keep CSI/DRA setup together instead of scattering it across every
//! runner.

use anyhow::{bail, Context, Result};
use k8s_openapi::api::apps::v1::{DaemonSet, Deployment, ReplicaSet, StatefulSet};
use k8s_openapi::api::batch::v1::{CronJob, Job};
use k8s_openapi::api::core::v1::{Endpoints, Namespace, Node, PersistentVolumeClaim, Pod, ServiceAccount};
use kube::api::{Api, DeleteParams, ListParams, Patch, PatchParams, PostParams};
use kube::Client;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use serde_json::json;
use std::net::IpAddr;
use std::path::Path;
use std::time::{Duration, Instant};
use std::future::Future;

const CSI_DRA_SHARDS: usize = 2;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TestGroup {
    General,
    CsiDra,
}

#[derive(Clone, Copy, Debug)]
struct TestCase {
    name: &'static str,
    group: TestGroup,
}

const TESTS: &[TestCase] = &[
    TestCase { name: "apiserver_serves_resources", group: TestGroup::General },
    TestCase { name: "node_is_ready", group: TestGroup::General },
    TestCase { name: "kubernetes_service_has_reachable_endpoint", group: TestGroup::General },
    TestCase { name: "test_job_controller_runs_pods_to_completion", group: TestGroup::General },
    TestCase { name: "test_job_controller_fails_after_backoff_limit", group: TestGroup::General },
    TestCase { name: "test_cronjob_controller_creates_a_job_on_schedule", group: TestGroup::General },
    TestCase { name: "test_ttl_after_finished_controller_deletes_expired_jobs", group: TestGroup::General },
    TestCase { name: "test_daemonset_places_a_pod_directly", group: TestGroup::General },
    TestCase { name: "test_deployment_creates_replicaset_and_rolls_update", group: TestGroup::General },
    TestCase { name: "test_replicaset_creates_and_scales_pods", group: TestGroup::General },
    TestCase { name: "test_statefulset_creates_ordinal_pods_and_scales_down_highest_first", group: TestGroup::General },
    TestCase { name: "test_statefulset_with_a_volume_claim_template_creates_an_accepted_pod", group: TestGroup::General },
];

#[derive(Clone)]
struct E2eContext {
    client: Client,
    namespace: String,
}

impl E2eContext {
    async fn create(client: Client) -> Result<Self> {
        let namespace = format!("nodebootstrap-e2e-{}-{}", std::process::id(), unique_suffix());
        let namespaces: Api<Namespace> = Api::all(client.clone());
        namespaces
            .create(
                &PostParams::default(),
                &Namespace {
                    metadata: ObjectMeta { name: Some(namespace.clone()), ..Default::default() },
                    ..Default::default()
                },
            )
            .await
            .with_context(|| format!("creating e2e namespace {namespace}"))?;

        let service_accounts: Api<ServiceAccount> = Api::namespaced(client.clone(), &namespace);
        let context = Self { client, namespace };
        context.wait_until("the e2e namespace's default ServiceAccount", Duration::from_secs(30), || {
            let service_accounts = service_accounts.clone();
            async move { Ok(service_accounts.get_opt("default").await?.is_some()) }
        })
        .await?;

        Ok(context)
    }

    async fn wait_until<F, Fut>(&self, description: &str, timeout: Duration, mut check: F) -> Result<()>
    where
        F: FnMut() -> Fut,
        Fut: Future<Output = Result<bool>>,
    {
        let deadline = Instant::now() + timeout;
        loop {
            if check().await? {
                return Ok(());
            }
            if Instant::now() >= deadline {
                bail!("timed out waiting for {description}");
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }

    async fn cleanup(&self) {
        let namespaces: Api<Namespace> = Api::all(self.client.clone());
        let _ = namespaces.delete(&self.namespace, &DeleteParams::default()).await;
    }
}

fn unique_suffix() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default()
}

fn labels(value: &str) -> ListParams {
    ListParams::default().labels(value)
}

/// Run the selected bootstrap-native checks without re-running installation
/// or re-executing through sudo. This mode is deliberately safe to invoke on
/// an already-running cluster as an ordinary user.
pub fn run(only: Option<&str>, shard: Option<&str>) -> Result<()> {
    select_kubeconfig()?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("building the bootstrap e2e runtime")?;
    runtime.block_on(run_async(only, shard))
}

async fn run_async(only: Option<&str>, shard: Option<&str>) -> Result<()> {
    let selected = select_tests(only, shard)?;
    if selected.is_empty() {
        println!("bootstrap e2e: no tests selected for this shard");
        return Ok(());
    }
    let client = Client::try_default()
        .await
        .context("loading the Kubernetes client for bootstrap e2e; set KUBECONFIG or bootstrap the cluster first")?;
    let context = E2eContext::create(client).await?;

    if let Some(shard) = shard {
        println!("bootstrap e2e: {} test(s), shard {shard}", selected.len());
    } else {
        println!("bootstrap e2e: {} test(s)", selected.len());
    }
    let mut failures = Vec::new();
    let mut passed = 0;
    for name in selected {
        let started = Instant::now();
        print!("▶ {name} ... ");
        match run_test(name, &context).await {
            Ok(()) => {
                passed += 1;
                println!("PASS ({}ms)", started.elapsed().as_millis());
            }
            Err(error) => {
                println!("FAIL ({}ms)", started.elapsed().as_millis());
                eprintln!("    {error:#}");
                failures.push(name);
            }
        }
    }

    context.cleanup().await;
    if failures.is_empty() {
        println!("Results: {passed} passed, 0 failed");
        Ok(())
    } else {
        bail!(
            "bootstrap e2e failed: {} test(s): {}",
            failures.len(),
            failures.join(", ")
        )
    }
}

/// Prefer an explicitly supplied kubeconfig. A nodebootstrap-created cluster
/// has a stable fallback path, so `./bootstrap --e2e` works immediately after
/// installation without requiring the caller to export an implementation-
/// specific k3s path.
fn select_kubeconfig() -> Result<()> {
    if std::env::var_os("KUBECONFIG").is_some_and(|value| !value.is_empty()) {
        return Ok(());
    }

    let cfg = crate::config::Config::from_env()?;
    let candidate = cfg.kubeconfig_dir().join("admin.kubeconfig");
    if Path::new(&candidate).is_file() {
        std::env::set_var("KUBECONFIG", &candidate);
        tracing::info!(path = %candidate.display(), "using nodebootstrap admin kubeconfig for e2e");
    }
    Ok(())
}

fn select_tests(only: Option<&str>, shard: Option<&str>) -> Result<Vec<&'static str>> {
    let shard = shard.map(parse_shard).transpose()?;
    let patterns: Vec<&str> = only
        .unwrap_or_default()
        .split(',')
        .filter(|pattern| !pattern.is_empty())
        .collect();

    if let Some(only) = only {
        let matches_any = TESTS.iter().any(|test| patterns.iter().any(|pattern| test.name.contains(pattern)));
        if !matches_any {
            bail!(
                "--only={only} selected no bootstrap e2e tests; available tests: {}",
                test_names().join(", ")
            );
        }
    }

    let mut general_position = 0;
    let mut csi_dra_position = 0;
    let mut selected = Vec::new();
    for test in TESTS {
        let selected_for_shard = match shard {
            None => true,
            Some(shard) => match test.group {
                TestGroup::General => {
                    let selected = assigned_to_shard(test.group, general_position, shard);
                    general_position += 1;
                    selected
                }
                TestGroup::CsiDra => {
                    let selected = assigned_to_shard(test.group, csi_dra_position, shard);
                    csi_dra_position += 1;
                    selected
                }
            },
        };
        if selected_for_shard && (only.is_none() || patterns.iter().any(|pattern| test.name.contains(pattern))) {
            selected.push(test.name);
        }
    }
    Ok(selected)
}

fn assigned_to_shard(group: TestGroup, position: usize, shard: Shard) -> bool {
    match group {
        TestGroup::General => position % shard.total == shard.index - 1,
        TestGroup::CsiDra => shard.index <= CSI_DRA_SHARDS && position % CSI_DRA_SHARDS == shard.index - 1,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Shard {
    index: usize,
    total: usize,
}

fn parse_shard(value: &str) -> Result<Shard> {
    let (index, total) = value
        .split_once('/')
        .with_context(|| format!("invalid --shard={value}; expected N/5"))?;
    let index = index.parse::<usize>().with_context(|| format!("invalid shard index in --shard={value}"))?;
    let total = total.parse::<usize>().with_context(|| format!("invalid shard total in --shard={value}"))?;
    anyhow::ensure!(total > 0 && index > 0 && index <= total, "invalid --shard={value}; expected 1 <= N <= total");
    anyhow::ensure!(total == 5, "invalid --shard={value}; CI uses exactly five e2e shards");
    Ok(Shard { index, total })
}

fn test_names() -> Vec<&'static str> {
    TESTS.iter().map(|test| test.name).collect()
}

async fn run_test(name: &str, context: &E2eContext) -> Result<()> {
    match name {
        "apiserver_serves_resources" => apiserver_serves_resources(context.client.clone()).await,
        "node_is_ready" => node_is_ready(context.client.clone()).await,
        "kubernetes_service_has_reachable_endpoint" => kubernetes_service_has_reachable_endpoint(context.client.clone()).await,
        "test_job_controller_runs_pods_to_completion" => job_controller_runs_pods_to_completion(context).await,
        "test_job_controller_fails_after_backoff_limit" => job_controller_fails_after_backoff_limit(context).await,
        "test_cronjob_controller_creates_a_job_on_schedule" => cronjob_controller_creates_a_job_on_schedule(context).await,
        "test_ttl_after_finished_controller_deletes_expired_jobs" => ttl_after_finished_controller_deletes_expired_jobs(context).await,
        "test_daemonset_places_a_pod_directly" => daemonset_places_a_pod_directly(context).await,
        "test_deployment_creates_replicaset_and_rolls_update" => deployment_creates_replicaset_and_rolls_update(context).await,
        "test_replicaset_creates_and_scales_pods" => replicaset_creates_and_scales_pods(context).await,
        "test_statefulset_creates_ordinal_pods_and_scales_down_highest_first" => statefulset_creates_ordinal_pods_and_scales_down_highest_first(context).await,
        "test_statefulset_with_a_volume_claim_template_creates_an_accepted_pod" => statefulset_with_a_volume_claim_template_creates_an_accepted_pod(context).await,
        other => bail!("unknown bootstrap e2e test {other}"),
    }
}

async fn job_controller_runs_pods_to_completion(context: &E2eContext) -> Result<()> {
    let name = "job-controller-completion";
    let jobs: Api<Job> = Api::namespaced(context.client.clone(), &context.namespace);
    let job: Job = serde_json::from_value(json!({
        "apiVersion": "batch/v1", "kind": "Job",
        "metadata": {"name": name},
        "spec": {
            "completions": 2, "parallelism": 2, "backoffLimit": 2,
            "template": {"spec": {"restartPolicy": "Never", "containers": [{
                "name": "busybox", "image": "busybox:latest", "command": ["sh", "-c", "exit 0"]
            }]}}
        }
    }))?;
    jobs.create(&PostParams::default(), &job).await.context("creating completion Job")?;
    context
        .wait_until("Job to report two succeeded Pods", Duration::from_secs(90), || {
            let jobs = jobs.clone();
            async move { Ok(jobs.get(name).await?.status.and_then(|status| status.succeeded) == Some(2)) }
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

async fn job_controller_fails_after_backoff_limit(context: &E2eContext) -> Result<()> {
    let name = "job-controller-failure";
    let jobs: Api<Job> = Api::namespaced(context.client.clone(), &context.namespace);
    let job: Job = serde_json::from_value(json!({
        "apiVersion": "batch/v1", "kind": "Job",
        "metadata": {"name": name},
        "spec": {
            "backoffLimit": 0,
            "template": {"spec": {"restartPolicy": "Never", "containers": [{
                "name": "busybox", "image": "busybox:latest", "command": ["sh", "-c", "exit 1"]
            }]}}
        }
    }))?;
    jobs.create(&PostParams::default(), &job).await.context("creating failing Job")?;
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

async fn cronjob_controller_creates_a_job_on_schedule(context: &E2eContext) -> Result<()> {
    let name = "cronjob-controller";
    let cronjobs: Api<CronJob> = Api::namespaced(context.client.clone(), &context.namespace);
    let jobs: Api<Job> = Api::namespaced(context.client.clone(), &context.namespace);
    let cronjob: CronJob = serde_json::from_value(json!({
        "apiVersion": "batch/v1", "kind": "CronJob",
        "metadata": {"name": name},
        "spec": {
            "schedule": "* * * * *", "concurrencyPolicy": "Allow",
            "jobTemplate": {"spec": {"template": {"spec": {"restartPolicy": "Never", "containers": [{
                "name": "busybox", "image": "busybox:latest", "command": ["sh", "-c", "exit 0"]
            }]}}}}
        }
    }))?;
    cronjobs.create(&PostParams::default(), &cronjob).await.context("creating CronJob")?;
    context
        .wait_until("CronJob to create a Job", Duration::from_secs(150), || {
            let jobs = jobs.clone();
            async move { Ok(!jobs.list(&labels(&format!("cronjob-name={name}"))).await?.items.is_empty()) }
        })
        .await?;
    context
        .wait_until("CronJob lastScheduleTime", Duration::from_secs(30), || {
            let cronjobs = cronjobs.clone();
            async move { Ok(cronjobs.get(name).await?.status.and_then(|status| status.last_schedule_time).is_some()) }
        })
        .await?;
    let _ = cronjobs.delete(name, &DeleteParams::default()).await;
    let _ = jobs.delete_collection(&DeleteParams::default(), &labels(&format!("cronjob-name={name}"))).await;
    Ok(())
}

async fn ttl_after_finished_controller_deletes_expired_jobs(context: &E2eContext) -> Result<()> {
    let name = "job-controller-ttl";
    let jobs: Api<Job> = Api::namespaced(context.client.clone(), &context.namespace);
    let job: Job = serde_json::from_value(json!({
        "apiVersion": "batch/v1", "kind": "Job",
        "metadata": {"name": name},
        "spec": {
            "ttlSecondsAfterFinished": 5, "backoffLimit": 0,
            "template": {"spec": {"restartPolicy": "Never", "containers": [{
                "name": "busybox", "image": "busybox:latest", "command": ["sh", "-c", "exit 0"]
            }]}}
        }
    }))?;
    jobs.create(&PostParams::default(), &job).await.context("creating TTL Job")?;
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
        .wait_until("TTL controller to delete the finished Job", Duration::from_secs(60), || {
            let jobs = jobs.clone();
            async move { Ok(jobs.get_opt(name).await?.is_none()) }
        })
        .await
}

async fn daemonset_places_a_pod_directly(context: &E2eContext) -> Result<()> {
    let name = "daemonset-controller";
    let daemonsets: Api<DaemonSet> = Api::namespaced(context.client.clone(), &context.namespace);
    let pods: Api<Pod> = Api::namespaced(context.client.clone(), &context.namespace);
    let nodes: Api<Node> = Api::all(context.client.clone());
    let node_name = nodes.list(&ListParams::default()).await?.items.into_iter().next().and_then(|node| node.metadata.name).context("finding a node for the DaemonSet")?;
    let daemonset: DaemonSet = serde_json::from_value(json!({
        "apiVersion": "apps/v1", "kind": "DaemonSet",
        "metadata": {"name": name},
        "spec": {"selector": {"matchLabels": {"app": name}}, "template": {
            "metadata": {"labels": {"app": name}},
            "spec": {"containers": [{"name": "busybox", "image": "busybox:latest", "command": ["sleep", "3600"]}]}
        }}
    }))?;
    daemonsets.create(&PostParams::default(), &daemonset).await.context("creating DaemonSet")?;
    context
        .wait_until("DaemonSet Pod to receive the node name", Duration::from_secs(60), || {
            let pods = pods.clone();
            let node_name = node_name.clone();
            async move { Ok(pods.list(&labels(&format!("app={name}"))).await?.items.into_iter().next().and_then(|pod| pod.spec.and_then(|spec| spec.node_name)) == Some(node_name)) }
        })
        .await?;
    context
        .wait_until("DaemonSet numberReady=1", Duration::from_secs(90), || {
            let daemonsets = daemonsets.clone();
            async move { Ok(daemonsets.get(name).await?.status.is_some_and(|status| status.number_ready == 1)) }
        })
        .await?;
    context
        .wait_until("DaemonSet desiredNumberScheduled=1", Duration::from_secs(30), || {
            let daemonsets = daemonsets.clone();
            async move { Ok(daemonsets.get(name).await?.status.is_some_and(|status| status.desired_number_scheduled == 1)) }
        })
        .await?;
    let _ = daemonsets.delete(name, &DeleteParams::default()).await;
    Ok(())
}

async fn deployment_creates_replicaset_and_rolls_update(context: &E2eContext) -> Result<()> {
    let name = "deployment-controller";
    let deployments: Api<Deployment> = Api::namespaced(context.client.clone(), &context.namespace);
    let replicasets: Api<ReplicaSet> = Api::namespaced(context.client.clone(), &context.namespace);
    let pods: Api<Pod> = Api::namespaced(context.client.clone(), &context.namespace);
    let deployment: Deployment = serde_json::from_value(json!({
        "apiVersion": "apps/v1", "kind": "Deployment",
        "metadata": {"name": name},
        "spec": {"replicas": 2, "selector": {"matchLabels": {"app": name}}, "template": {
            "metadata": {"labels": {"app": name}},
            "spec": {"containers": [{"name": "busybox", "image": "busybox:latest", "command": ["sleep", "3600"]}]}
        }}
    }))?;
    deployments.create(&PostParams::default(), &deployment).await.context("creating Deployment")?;
    context
        .wait_until("Deployment to create one ReplicaSet", Duration::from_secs(60), || {
            let replicasets = replicasets.clone();
            async move { Ok(replicasets.list(&labels(&format!("app={name}"))).await?.items.len() == 1) }
        })
        .await?;
    context
        .wait_until("Deployment to create two Pods", Duration::from_secs(60), || {
            let pods = pods.clone();
            async move { Ok(pods.list(&labels(&format!("app={name}"))).await?.items.len() == 2) }
        })
        .await?;
    context
        .wait_until("Deployment readyReplicas=2", Duration::from_secs(90), || {
            let deployments = deployments.clone();
            async move { Ok(deployments.get(name).await?.status.and_then(|status| status.ready_replicas) == Some(2)) }
        })
        .await?;
    let patch = json!({"spec": {"template": {"spec": {"containers": [{"name": "busybox", "image": "busybox:latest", "command": ["sleep", "7200"]}]}}}});
    deployments.patch(name, &PatchParams::default(), &Patch::Merge(&patch)).await.context("patching Deployment template")?;
    context
        .wait_until("Deployment to create a second ReplicaSet", Duration::from_secs(90), || {
            let replicasets = replicasets.clone();
            async move { Ok(replicasets.list(&labels(&format!("app={name}"))).await?.items.len() >= 2) }
        })
        .await?;
    context
        .wait_until("Deployment to retain two Pods after rollout", Duration::from_secs(90), || {
            let pods = pods.clone();
            async move { Ok(pods.list(&labels(&format!("app={name}"))).await?.items.len() == 2) }
        })
        .await
}

async fn replicaset_creates_and_scales_pods(context: &E2eContext) -> Result<()> {
    let name = "replicaset-controller";
    let replicasets: Api<ReplicaSet> = Api::namespaced(context.client.clone(), &context.namespace);
    let pods: Api<Pod> = Api::namespaced(context.client.clone(), &context.namespace);
    let replicaset: ReplicaSet = serde_json::from_value(json!({
        "apiVersion": "apps/v1", "kind": "ReplicaSet",
        "metadata": {"name": name},
        "spec": {"replicas": 2, "selector": {"matchLabels": {"app": name}}, "template": {
            "metadata": {"labels": {"app": name}},
            "spec": {"containers": [{"name": "busybox", "image": "busybox:latest", "command": ["sleep", "3600"]}]}
        }}
    }))?;
    replicasets.create(&PostParams::default(), &replicaset).await.context("creating ReplicaSet")?;
    context
        .wait_until("ReplicaSet to create two Pods", Duration::from_secs(60), || {
            let pods = pods.clone();
            async move { Ok(pods.list(&labels(&format!("app={name}"))).await?.items.len() == 2) }
        })
        .await?;
    context
        .wait_until("ReplicaSet readyReplicas=2", Duration::from_secs(90), || {
            let replicasets = replicasets.clone();
            async move { Ok(replicasets.get(name).await?.status.and_then(|status| status.ready_replicas) == Some(2)) }
        })
        .await?;
    let patch = json!({"spec": {"replicas": 1}});
    replicasets.patch(name, &PatchParams::default(), &Patch::Merge(&patch)).await.context("scaling ReplicaSet")?;
    context
        .wait_until("ReplicaSet to scale down to one Pod", Duration::from_secs(60), || {
            let pods = pods.clone();
            async move { Ok(pods.list(&labels(&format!("app={name}"))).await?.items.len() == 1) }
        })
        .await
}

async fn statefulset_creates_ordinal_pods_and_scales_down_highest_first(context: &E2eContext) -> Result<()> {
    let name = "statefulset-controller";
    let statefulsets: Api<StatefulSet> = Api::namespaced(context.client.clone(), &context.namespace);
    let pods: Api<Pod> = Api::namespaced(context.client.clone(), &context.namespace);
    let statefulset: StatefulSet = serde_json::from_value(json!({
        "apiVersion": "apps/v1", "kind": "StatefulSet",
        "metadata": {"name": name}, "spec": {
            "serviceName": name, "replicas": 2,
            "selector": {"matchLabels": {"app": name}},
            "template": {"metadata": {"labels": {"app": name}}, "spec": {
                "containers": [{"name": "busybox", "image": "busybox:latest", "command": ["sh", "-c", "sleep 15; touch /tmp/release; sleep 3600"],
                    "readinessProbe": {"exec": {"command": ["test", "-f", "/tmp/release"]}, "periodSeconds": 1}]}
            }}
        }
    }))?;
    statefulsets.create(&PostParams::default(), &statefulset).await.context("creating StatefulSet")?;
    context
        .wait_until("StatefulSet ordinal zero", Duration::from_secs(60), || {
            let pods = pods.clone();
            async move { Ok(pods.get_opt(&format!("{name}-0")).await?.is_some()) }
        })
        .await?;
    context
        .wait_until("StatefulSet ordinal zero to be Running but initially unready", Duration::from_secs(30), || {
            let pods = pods.clone();
            async move {
                let pod = pods.get(&format!("{name}-0")).await?;
                let running = pod.status.as_ref().and_then(|status| status.phase.as_deref()) == Some("Running");
                let ready = pod.status.and_then(|status| status.conditions).unwrap_or_default().iter().any(|condition| condition.type_ == "Ready" && condition.status == "True");
                Ok(running && !ready)
            }
        })
        .await?;
    anyhow::ensure!(pods.get_opt(&format!("{name}-1")).await?.is_none(), "OrderedReady created ordinal one before ordinal zero became ready");
    context
        .wait_until("StatefulSet ordinal one after ordinal zero is ready", Duration::from_secs(90), || {
            let pods = pods.clone();
            async move { Ok(pods.get_opt(&format!("{name}-1")).await?.is_some()) }
        })
        .await?;
    context
        .wait_until("StatefulSet to report two ready replicas", Duration::from_secs(90), || {
            let statefulsets = statefulsets.clone();
            async move { Ok(statefulsets.get(name).await?.status.and_then(|status| status.ready_replicas) == Some(2)) }
        })
        .await?;
    let patch = json!({"spec": {"replicas": 1}});
    statefulsets.patch(name, &PatchParams::default(), &Patch::Merge(&patch)).await.context("scaling StatefulSet")?;
    context
        .wait_until("StatefulSet to delete the highest ordinal first", Duration::from_secs(60), || {
            let pods = pods.clone();
            async move { Ok(pods.get_opt(&format!("{name}-1")).await?.is_none()) }
        })
        .await?;
    anyhow::ensure!(pods.get_opt(&format!("{name}-0")).await?.is_some(), "StatefulSet deleted ordinal zero instead of the highest ordinal");
    let _ = statefulsets.delete(name, &DeleteParams::default()).await;
    Ok(())
}

async fn statefulset_with_a_volume_claim_template_creates_an_accepted_pod(context: &E2eContext) -> Result<()> {
    let name = "statefulset-controller-pvc";
    let statefulsets: Api<StatefulSet> = Api::namespaced(context.client.clone(), &context.namespace);
    let pods: Api<Pod> = Api::namespaced(context.client.clone(), &context.namespace);
    let pvcs: Api<PersistentVolumeClaim> = Api::namespaced(context.client.clone(), &context.namespace);
    let statefulset: StatefulSet = serde_json::from_value(json!({
        "apiVersion": "apps/v1", "kind": "StatefulSet",
        "metadata": {"name": name}, "spec": {
            "serviceName": name, "replicas": 1,
            "selector": {"matchLabels": {"app": name}},
            "template": {"metadata": {"labels": {"app": name}}, "spec": {
                "containers": [{"name": "busybox", "image": "busybox:latest", "command": ["sleep", "3600"], "volumeMounts": [{"name": "data", "mountPath": "/data"}]}]
            }},
            "volumeClaimTemplates": [{"metadata": {"name": "data"}, "spec": {"accessModes": ["ReadWriteOnce"], "resources": {"requests": {"storage": "64Mi"}}}}]
        }
    }))?;
    statefulsets.create(&PostParams::default(), &statefulset).await.context("creating StatefulSet with a volume claim template")?;
    let pod_name = format!("{name}-0");
    context
        .wait_until("StatefulSet PVC-backed Pod", Duration::from_secs(60), || {
            let pods = pods.clone();
            let pod_name = pod_name.clone();
            async move { Ok(pods.get_opt(&pod_name).await?.is_some()) }
        })
        .await?;
    let pod = pods.get(&pod_name).await?;
    let volume = pod
        .spec
        .as_ref()
        .and_then(|spec| spec.volumes.as_ref())
        .into_iter()
        .flatten()
        .find(|volume| volume.name == "data")
        .context("StatefulSet Pod is missing the injected data volume")?;
    anyhow::ensure!(
        volume.persistent_volume_claim.as_ref().is_some_and(|claim| claim.claim_name == "data-statefulset-controller-pvc-0"),
        "StatefulSet Pod volume must reference the generated data-statefulset-controller-pvc-0 claim"
    );
    let _ = statefulsets.delete(name, &DeleteParams::default()).await;
    let _ = pvcs.delete_collection(&DeleteParams::default(), &ListParams::default()).await;
    Ok(())
}

async fn apiserver_serves_resources(client: Client) -> Result<()> {
    let api: Api<Namespace> = Api::all(client);
    let namespaces = api
        .list(&ListParams::default())
        .await
        .context("listing namespaces")?;
    anyhow::ensure!(!namespaces.items.is_empty(), "the apiserver returned no namespaces");
    Ok(())
}

async fn node_is_ready(client: Client) -> Result<()> {
    let api: Api<Node> = Api::all(client);
    let nodes = api.list(&ListParams::default()).await.context("listing nodes")?;
    anyhow::ensure!(!nodes.items.is_empty(), "the apiserver returned no nodes");

    let ready = nodes.items.iter().filter(|node| {
        node.status
            .as_ref()
            .and_then(|status| status.conditions.as_ref())
            .is_some_and(|conditions| conditions.iter().any(|condition| condition.type_ == "Ready" && condition.status == "True"))
    });
    let ready_count = ready.count();
    anyhow::ensure!(ready_count > 0, "no node reported status.conditions[Ready]=True");
    Ok(())
}

async fn kubernetes_service_has_reachable_endpoint(client: Client) -> Result<()> {
    let api: Api<Endpoints> = Api::namespaced(client, "default");
    let endpoints = api
        .get("kubernetes")
        .await
        .context("reading default/kubernetes Endpoints")?;

    let mut addresses = Vec::new();
    for subset in endpoints.subsets.unwrap_or_default() {
        for address in subset.addresses.unwrap_or_default() {
            addresses.push(address.ip);
        }
    }

    let reachable = addresses.iter().filter_map(|address| address.parse::<IpAddr>().ok()).any(|ip| !ip.is_loopback() && !ip.is_unspecified());
    anyhow::ensure!(reachable, "default/kubernetes has no non-loopback, non-unspecified endpoint (addresses: {addresses:?})");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_filter_selects_the_initial_bootstrap_checks() {
        assert_eq!(select_tests(None, None).unwrap(), test_names());
    }

    #[test]
    fn only_matches_test_name_substrings_and_comma_separates() {
        assert_eq!(
            select_tests(Some("node_is_ready,kubernetes_service"), None).unwrap(),
            vec!["node_is_ready", "kubernetes_service_has_reachable_endpoint"]
        );
    }

    #[test]
    fn an_unknown_only_pattern_is_an_error() {
        assert!(select_tests(Some("does_not_exist"), None).is_err());
    }

    #[test]
    fn general_tests_are_round_robined_across_five_shards() {
        assert_eq!(select_tests(None, Some("1/5")).unwrap(), vec!["apiserver_serves_resources", "test_cronjob_controller_creates_a_job_on_schedule"]);
        assert_eq!(select_tests(None, Some("2/5")).unwrap(), vec!["node_is_ready", "test_ttl_after_finished_controller_deletes_expired_jobs"]);
        assert_eq!(select_tests(None, Some("3/5")).unwrap(), vec!["kubernetes_service_has_reachable_endpoint", "test_daemonset_places_a_pod_directly"]);
        assert_eq!(select_tests(None, Some("4/5")).unwrap(), vec!["test_job_controller_runs_pods_to_completion", "test_deployment_creates_replicaset_and_rolls_update"]);
        assert_eq!(select_tests(None, Some("5/5")).unwrap(), vec!["test_job_controller_fails_after_backoff_limit", "test_replicaset_creates_and_scales_pods"]);
    }

    #[test]
    fn shard_parser_requires_the_five_way_ci_layout() {
        assert_eq!(parse_shard("2/5").unwrap(), Shard { index: 2, total: 5 });
        assert!(parse_shard("0/5").is_err());
        assert!(parse_shard("1/4").is_err());
    }

    #[test]
    fn csi_and_dra_tests_only_use_the_first_two_shards() {
        let shard_one = Shard { index: 1, total: 5 };
        let shard_two = Shard { index: 2, total: 5 };
        let shard_three = Shard { index: 3, total: 5 };
        assert!(assigned_to_shard(TestGroup::CsiDra, 0, shard_one));
        assert!(assigned_to_shard(TestGroup::CsiDra, 1, shard_two));
        assert!(!assigned_to_shard(TestGroup::CsiDra, 0, shard_three));
        assert!(!assigned_to_shard(TestGroup::CsiDra, 1, shard_three));
    }
}
