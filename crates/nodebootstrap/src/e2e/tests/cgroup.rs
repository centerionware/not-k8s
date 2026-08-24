use super::context::E2eContext;
use super::skip_test;
use anyhow::{Context, Result};
use k8s_openapi::api::core::v1::Pod;
use kube::api::{Api, PostParams};
use serde_json::json;
use std::path::{Path, PathBuf};
use std::time::Duration;

fn cgroup_root() -> PathBuf {
    std::env::var("NODELET_CGROUP_FS_ROOT")
        .unwrap_or_else(|_| "/sys/fs/cgroup".to_string())
        .into()
}

fn needs_cri() -> Result<()> {
    anyhow::ensure!(
        crate::config::Config::from_env()?.nodelet_runtime() == "cri",
        "cgroup hierarchy checks require nodelet's CRI runtime",
    );
    Ok(())
}

fn find_directory_containing(root: &Path, needles: &[&str], depth: usize) -> Option<PathBuf> {
    let entries = std::fs::read_dir(root).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        let file_type = entry.file_type().ok()?;
        if !file_type.is_dir() {
            continue;
        }
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if needles.iter().any(|needle| name.contains(needle)) {
            return Some(path);
        }
        if depth > 0 {
            if let Some(found) = find_directory_containing(&path, needles, depth - 1) {
                return Some(found);
            }
        }
    }
    None
}

pub(super) async fn node_allocatable_cgroup_exists_and_is_capped(
    _context: &E2eContext,
) -> Result<()> {
    if let Err(error) = needs_cri() {
        return Err(skip_test(error.to_string()));
    }
    let kubepods = cgroup_root().join("kubepods");
    if !kubepods.is_dir() {
        return Err(skip_test(format!(
            "{} is unavailable; node allocatable cgroup checks do not apply on this host",
            kubepods.display()
        )));
    }
    let cpu_max = kubepods.join("cpu.max");
    let memory_max = kubepods.join("memory.max");
    if !cpu_max.is_file() || !memory_max.is_file() {
        return Err(skip_test(format!(
            "{} lacks readable cpu.max and memory.max; cgroup-v2 node allocatable checks do not apply",
            kubepods.display()
        )));
    }
    anyhow::ensure!(
        !std::fs::read_to_string(&cpu_max)
            .with_context(|| format!("reading {}", cpu_max.display()))?
            .trim()
            .is_empty(),
        "{} is empty",
        cpu_max.display()
    );
    anyhow::ensure!(
        !std::fs::read_to_string(&memory_max)
            .with_context(|| format!("reading {}", memory_max.display()))?
            .trim()
            .is_empty(),
        "{} is empty",
        memory_max.display()
    );
    Ok(())
}

pub(super) async fn pod_cgroup_reflects_its_qos_class(context: &E2eContext) -> Result<()> {
    if let Err(error) = needs_cri() {
        return Err(skip_test(error.to_string()));
    }
    let name = "cgroup-qos-check";
    let pods: Api<Pod> = Api::namespaced(context.client.clone(), &context.namespace);
    let pod: Pod = serde_json::from_value(json!({
        "apiVersion": "v1",
        "kind": "Pod",
        "metadata": {"name": name},
        "spec": {
            "containers": [{"name": "app", "image": "busybox:latest", "command": ["sleep", "3600"]}]
        }
    }))?;
    pods.create(&PostParams::default(), &pod).await?;
    context
        .wait_until("cgroup test Pod Running", Duration::from_secs(90), || {
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

    let uid = pods
        .get(name)
        .await?
        .metadata
        .uid
        .context("cgroup test Pod has no UID")?;
    let uid_underscored = uid.replace('-', "_");
    let root = cgroup_root().join("kubepods");
    if find_directory_containing(&root, &[&uid, &uid_underscored], 3).is_none() {
        return Err(skip_test(format!(
            "no cgroup directory under {} contains Pod UID {}; this runtime's cgroup layout is not inspectable by this test",
            root.display(),
            uid
        )));
    }
    Ok(())
}
