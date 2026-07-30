//! `/stats/summary` — real kubelet's Summary API
//! (`k8s.io/kubelet/pkg/apis/stats/v1alpha1`), what metrics-server scrapes
//! to back `kubectl top node`/`kubectl top pod`. Built from CRI's own
//! `ListPodSandboxStats` (per-pod *and* per-container CPU/memory usage in
//! one call) rather than nodelet reading cgroup files itself — CRI already
//! solved "where's this container's cgroup" for every driver/runtime
//! combination, no reason to redo it.
//!
//! **Caveat, not a nodelet limitation**: `kubectl top` itself talks to the
//! `metrics.k8s.io` aggregated API, which only exists if metrics-server (or
//! another Metrics API implementation) is deployed *and configured to
//! scrape this endpoint*. Implementing `/stats/summary` is necessary for
//! `kubectl top` to work, not sufficient on its own.

use super::{BoxedBody, ServerState};
use crate::runtime::{PodUsage, UsageStats};
use hyper::{Response, StatusCode};
use serde::Serialize;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Summary {
    pub node: NodeStats,
    pub pods: Vec<PodStats>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NodeStats {
    pub node_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cpu: Option<CpuStats>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memory: Option<MemoryStats>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PodStats {
    pub pod_ref: PodReference,
    pub containers: Vec<ContainerStats>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cpu: Option<CpuStats>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memory: Option<MemoryStats>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PodReference {
    pub name: String,
    pub namespace: String,
    pub uid: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ContainerStats {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cpu: Option<CpuStats>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memory: Option<MemoryStats>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CpuStats {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage_nano_cores: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage_core_nano_seconds: Option<u64>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MemoryStats {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub available_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub working_set_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rss_bytes: Option<u64>,
}

/// `None` if every field of `stats` is `None` — an all-empty CPU/Memory
/// block is noise, real kubelet omits the whole section too when a runtime
/// hasn't measured something yet.
fn cpu_stats(stats: &UsageStats) -> Option<CpuStats> {
    if stats.cpu_usage_nano_cores.is_none() && stats.cpu_usage_core_nano_seconds.is_none() {
        return None;
    }
    Some(CpuStats { usage_nano_cores: stats.cpu_usage_nano_cores, usage_core_nano_seconds: stats.cpu_usage_core_nano_seconds })
}

fn memory_stats(stats: &UsageStats) -> Option<MemoryStats> {
    if stats.memory_available_bytes.is_none()
        && stats.memory_usage_bytes.is_none()
        && stats.memory_working_set_bytes.is_none()
        && stats.memory_rss_bytes.is_none()
    {
        return None;
    }
    Some(MemoryStats {
        available_bytes: stats.memory_available_bytes,
        usage_bytes: stats.memory_usage_bytes,
        working_set_bytes: stats.memory_working_set_bytes,
        rss_bytes: stats.memory_rss_bytes,
    })
}

pub fn build_pod_stats(usage: &PodUsage) -> PodStats {
    PodStats {
        pod_ref: PodReference { name: usage.name.clone(), namespace: usage.namespace.clone(), uid: usage.uid.clone() },
        containers: usage
            .containers
            .iter()
            .map(|c| ContainerStats { name: c.name.clone(), cpu: cpu_stats(&c.stats), memory: memory_stats(&c.stats) })
            .collect(),
        cpu: cpu_stats(&usage.pod),
        memory: memory_stats(&usage.pod),
    }
}

/// Node-level memory from `/proc/meminfo` (same source as `metrics.rs`'s
/// `MemoryPressure` condition) — real usage, not just capacity. Node-level
/// CPU usage isn't populated: CRI's stats are per-container/per-pod, not
/// whole-node, and nodelet doesn't run its own cgroup/cAdvisor-style
/// node-wide CPU accounting (a real, separate remaining gap).
fn node_stats(node_name: &str) -> NodeStats {
    let memory = crate::metrics::read_mem_info().map(|m| MemoryStats {
        available_bytes: Some(m.available_bytes),
        usage_bytes: Some(m.total_bytes.saturating_sub(m.available_bytes)),
        working_set_bytes: None,
        rss_bytes: None,
    });
    NodeStats { node_name: node_name.to_string(), cpu: None, memory }
}

pub async fn handle_stats_summary(state: &ServerState) -> Response<BoxedBody> {
    let usages = match state.runtime.pod_usage_stats().await {
        Ok(u) => u,
        Err(e) => return super::text_response(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    };
    let summary =
        Summary { node: node_stats(&state.node_name), pods: usages.iter().map(build_pod_stats).collect() };
    let body = match serde_json::to_vec(&summary) {
        Ok(b) => b,
        Err(e) => return super::text_response(StatusCode::INTERNAL_SERVER_ERROR, format!("serializing summary: {e}")),
    };
    Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", "application/json")
        .body(super::body_from_bytes(body))
        .unwrap()
}

#[cfg(test)]
#[path = "stats_tests/build_pod_stats.rs"]
mod tests_build_pod_stats;
