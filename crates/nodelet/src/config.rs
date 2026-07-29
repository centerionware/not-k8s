//! Runtime configuration, resolved from environment variables.
//!
//! Everything has a sensible default so `nodelet` can run with zero config
//! against a kubeconfig-resolved cluster.

use anyhow::{Context, Result};
use std::collections::BTreeMap;
use std::time::Duration;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RuntimeKind {
    /// In-memory fake runtime: reports pods Running without a container engine.
    /// Lets you exercise/measure the full control loop with zero runtime overhead.
    Mock,
    /// Real containerd/CRI runtime (only available when built with `--features cri`).
    Cri,
}

#[derive(Clone, Debug)]
pub struct Config {
    pub node_name: String,
    pub runtime: RuntimeKind,
    #[cfg_attr(not(feature = "cri"), allow(dead_code))]
    pub cri_endpoint: String,
    /// Lease renewal interval — cheap liveness signal (default 10s).
    pub heartbeat: Duration,
    /// Node-status push interval — heavier, infrequent (default 60s).
    pub status_interval: Duration,
    pub cpu_cores: u64,
    pub memory_bytes: u64,
    pub max_pods: u64,
    pub labels: BTreeMap<String, String>,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        let node_name = env_or("NODELET_NODE_NAME", detect_hostname);

        let runtime = match std::env::var("NODELET_RUNTIME").as_deref() {
            Ok("cri") => RuntimeKind::Cri,
            Ok("mock") | Err(_) => RuntimeKind::Mock,
            Ok(other) => anyhow::bail!("unknown NODELET_RUNTIME '{other}' (want 'mock' or 'cri')"),
        };

        let cri_endpoint = std::env::var("NODELET_CRI_ENDPOINT")
            .unwrap_or_else(|_| "unix:///run/containerd/containerd.sock".to_string());

        let heartbeat = Duration::from_secs(env_u64("NODELET_HEARTBEAT_SECS", 10)?);
        let status_interval = Duration::from_secs(env_u64("NODELET_STATUS_SECS", 60)?);

        let cpu_cores = match std::env::var("NODELET_CPU") {
            Ok(v) => v.parse().context("NODELET_CPU must be an integer")?,
            Err(_) => detect_cpu_cores(),
        };

        let memory_bytes = match std::env::var("NODELET_MEMORY_BYTES") {
            Ok(v) => v.parse().context("NODELET_MEMORY_BYTES must be an integer")?,
            Err(_) => detect_memory_bytes(),
        };

        let max_pods = env_u64("NODELET_MAX_PODS", 110)?;

        let mut labels = BTreeMap::new();
        if let Ok(raw) = std::env::var("NODELET_LABELS") {
            for pair in raw.split(',').filter(|s| !s.trim().is_empty()) {
                let (k, v) = pair
                    .split_once('=')
                    .with_context(|| format!("bad label '{pair}', expected k=v"))?;
                labels.insert(k.trim().to_string(), v.trim().to_string());
            }
        }

        Ok(Self {
            node_name,
            runtime,
            cri_endpoint,
            heartbeat,
            status_interval,
            cpu_cores,
            memory_bytes,
            max_pods,
            labels,
        })
    }
}

fn env_or(key: &str, default: impl FnOnce() -> String) -> String {
    std::env::var(key).ok().filter(|s| !s.is_empty()).unwrap_or_else(default)
}

fn env_u64(key: &str, default: u64) -> Result<u64> {
    match std::env::var(key) {
        Ok(v) => v.parse().with_context(|| format!("{key} must be an integer")),
        Err(_) => Ok(default),
    }
}

fn detect_hostname() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .map(|s| s.trim().to_string())
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| std::env::var("HOSTNAME").ok())
        .unwrap_or_else(|| "nodelet".to_string())
}

fn detect_cpu_cores() -> u64 {
    std::thread::available_parallelism().map(|n| n.get() as u64).unwrap_or(1)
}

fn detect_memory_bytes() -> u64 {
    // Parse MemTotal (kB) from /proc/meminfo.
    if let Ok(meminfo) = std::fs::read_to_string("/proc/meminfo") {
        for line in meminfo.lines() {
            if let Some(rest) = line.strip_prefix("MemTotal:") {
                if let Some(kb) = rest.split_whitespace().next().and_then(|n| n.parse::<u64>().ok()) {
                    return kb * 1024;
                }
            }
        }
    }
    2 * 1024 * 1024 * 1024 // 2 GiB fallback
}
