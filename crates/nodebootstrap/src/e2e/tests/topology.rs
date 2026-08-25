use super::context::E2eContext;
use super::resource_managers::NodeletEnvOverride;
use super::skip_test;
use anyhow::Result;
use k8s_openapi::api::core::v1::Pod;
use kube::api::{Api, PostParams};
use serde_json::json;
use std::fs;
use std::time::Duration;

fn single_numa_node() -> bool {
    fs::read_dir("/sys/devices/system/node")
        .ok()
        .map(|entries| {
            entries
                .flatten()
                .filter(|entry| {
                    entry
                        .file_name()
                        .to_str()
                        .is_some_and(|name| name.starts_with("node"))
                })
                .count()
                == 1
        })
        .unwrap_or(false)
}

async fn run_single_numa_case(context: &E2eContext, policy: &str, name: &str) -> Result<()> {
    if crate::config::Config::from_env()?.nodelet_runtime() != "cri" {
        return Err(skip_test("topology-manager checks require the CRI runtime"));
    }
    if !single_numa_node() {
        return Err(skip_test(
            "this host does not expose exactly one NUMA node; the single-node topology case is not applicable",
        ));
    }
    let policy_env = [
        ("NODELET_TOPOLOGY_MANAGER_POLICY", policy),
        ("NODELET_CPU_MANAGER_POLICY", "static"),
    ];
    let _override = NodeletEnvOverride::install(&policy_env)?;
    let pods: Api<Pod> = Api::namespaced(context.client.clone(), &context.namespace);
    let pod: Pod = serde_json::from_value(json!({
        "apiVersion": "v1",
        "kind": "Pod",
        "metadata": {"name": name},
        "spec": {
            "containers": [{"name": "app", "image": "busybox:latest", "command": ["sleep", "3600"], "resources": {"requests": {"cpu": "1", "memory": "64Mi"}, "limits": {"cpu": "1", "memory": "64Mi"}}}]
        }
    }))?;
    pods.create(&PostParams::default(), &pod).await?;
    context
        .wait_until("single-NUMA topology Pod to reach Running", Duration::from_secs(150), || {
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

pub(super) async fn topology_manager_does_not_reject_pods_on_a_single_numa_node_host(
    context: &E2eContext,
) -> Result<()> {
    run_single_numa_case(context, "single-numa-node", "topology-single-numa").await
}

pub(super) async fn topology_manager_restricted_does_not_reject_pods_on_a_single_numa_node_host(
    context: &E2eContext,
) -> Result<()> {
    run_single_numa_case(context, "restricted", "topology-restricted").await
}

pub(super) async fn topology_manager_cross_provider_alignment_manual_note(
    _context: &E2eContext,
) -> Result<()> {
    Err(skip_test(
        "cross-provider CPU/device NUMA alignment needs real multi-NUMA hardware and a device plugin advertising NUMA affinity",
    ))
}

pub(super) async fn topology_manager_restricted_spread_manual_note(
    _context: &E2eContext,
) -> Result<()> {
    Err(skip_test(
        "Topology Manager restricted spread needs real multi-NUMA hardware with providers constrained to different nodes",
    ))
}
