//! Static pods + mirror pods.
//!
//! Real kubelet feature: manifests dropped into a directory (`staticPodPath`,
//! e.g. `/etc/kubernetes/manifests`) run directly on the node — no
//! scheduler, no apiserver round-trip for the desired spec — and kubelet
//! creates a read-only "mirror pod" in the apiserver so `kubectl get pods`
//! can see them. Deleting the file tears the pod down and removes the
//! mirror. Before this, nodelet had no such directory-watching at all.
//!
//! Disabled by default (`NODELET_STATIC_POD_PATH` unset) — matches real
//! kubelet, where `staticPodPath` is likewise optional.

use crate::config::Config;
use crate::runtime::PodRuntime;
use anyhow::{Context, Result};
use k8s_openapi::api::core::v1::Pod;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use kube::api::{Api, Patch, PatchParams};
use kube::Client;
use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tracing::warn;

/// Every regular file in `dir` not starting with `.` — kubelet's own rule
/// for what counts as a manifest in the static pod directory (no filtering
/// by extension). Sorted for deterministic scan order.
pub fn scan_manifest_dir(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut files: Vec<PathBuf> = entries
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_file())
        .filter(|e| !e.file_name().to_string_lossy().starts_with('.'))
        .map(|e| e.path())
        .collect();
    files.sort();
    files
}

/// Parse a manifest file's bytes as a Pod. YAML is a JSON superset, so this
/// handles both `.yaml` and `.json` manifests with one parser.
pub fn parse_manifest(bytes: &[u8]) -> Result<Pod> {
    serde_yaml::from_slice(bytes).context("parsing static pod manifest")
}

fn content_hash(bytes: &[u8]) -> u64 {
    let mut hasher = DefaultHasher::new();
    bytes.hash(&mut hasher);
    hasher.finish()
}

/// Real kubelet's mirror pod naming: `<pod-name>-<node-name>`.
pub fn mirror_pod_name(pod_name: &str, node_name: &str) -> String {
    format!("{pod_name}-{node_name}")
}

/// Bind a static pod's spec directly to this node — it skips the scheduler
/// entirely (there's no other node it could have been scheduled to; static
/// pods are inherently node-local).
pub fn prepare_static_pod(mut pod: Pod, node_name: &str) -> Pod {
    if pod.metadata.namespace.is_none() {
        pod.metadata.namespace = Some("default".to_string());
    }
    if let Some(spec) = pod.spec.as_mut() {
        spec.node_name = Some(node_name.to_string());
    }
    pod
}

/// Build the read-only mirror Pod object the apiserver sees, from an
/// already-`prepare_static_pod()`-ed pod. Annotated the same way real
/// kubelet marks mirror pods (`kubernetes.io/config.mirror`,
/// `kubernetes.io/config.source: file`) so tooling that checks for those
/// annotations still recognizes it as one, even though nodelet doesn't
/// replicate kubelet's exact hash-based drift-detection value.
pub fn build_mirror_pod(prepared_pod: &Pod, node_name: &str) -> Pod {
    let source_name = prepared_pod.metadata.name.clone().unwrap_or_default();
    let namespace = prepared_pod.metadata.namespace.clone().unwrap_or_else(|| "default".to_string());
    let mut annotations = prepared_pod.metadata.annotations.clone().unwrap_or_default();
    annotations.insert("kubernetes.io/config.source".to_string(), "file".to_string());
    annotations.insert("kubernetes.io/config.mirror".to_string(), "nodelet-static-pod".to_string());

    Pod {
        metadata: ObjectMeta {
            name: Some(mirror_pod_name(&source_name, node_name)),
            namespace: Some(namespace),
            labels: prepared_pod.metadata.labels.clone(),
            annotations: Some(annotations),
            ..Default::default()
        },
        spec: prepared_pod.spec.clone(),
        ..Default::default()
    }
}

/// Read+hash one manifest file, returning `None` (with the hash still
/// computed) when unchanged from `previous_hash` — the sync loop's signal
/// to skip re-`ensure_pod`-ing a pod whose manifest hasn't actually changed.
pub fn load_if_changed(path: &Path, previous_hash: Option<u64>, node_name: &str) -> Result<Option<(u64, Pod)>> {
    let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let hash = content_hash(&bytes);
    if previous_hash == Some(hash) {
        return Ok(None);
    }
    let pod = parse_manifest(&bytes)?;
    Ok(Some((hash, prepare_static_pod(pod, node_name))))
}

/// Per-manifest-path state the sync loop keeps across ticks.
struct TrackedManifest {
    hash: u64,
    prepared_pod: Pod,
    mirror_name: String,
    namespace: String,
}

/// Watch `cfg.static_pod_path` (if set — the feature is disabled entirely
/// otherwise, matching real kubelet's optional `staticPodPath`) and keep
/// each manifest's pod running + mirrored into the apiserver. Uses the same
/// `PodRuntime` the normal apiserver-sourced pods do, so resource limits,
/// securityContext, volumes, probes etc. all just work identically — the
/// only thing static pods actually need on top is *sourcing* (a directory
/// instead of a watch) and *mirroring* (a read-only Pod object so `kubectl
/// get pods` can see them).
pub async fn run(client: Client, runtime: Arc<dyn PodRuntime>, cfg: Config) {
    let Some(dir) = cfg.static_pod_path.clone() else { return };
    let host_ip = crate::node::detect_internal_ip();
    let health = crate::probes::new_health_map();
    let mut tracked: HashMap<PathBuf, TrackedManifest> = HashMap::new();

    loop {
        tokio::time::sleep(cfg.static_pod_sync_interval).await;
        let dir_path = Path::new(&dir);
        let files = scan_manifest_dir(dir_path);
        let mut seen = std::collections::HashSet::new();

        for path in &files {
            seen.insert(path.clone());
            let previous_hash = tracked.get(path).map(|t| t.hash);
            let loaded = match load_if_changed(path, previous_hash, &cfg.node_name) {
                Ok(loaded) => loaded,
                Err(e) => {
                    warn!(path = %path.display(), error = ?e, "static pod: failed to load manifest");
                    continue;
                }
            };
            let Some((hash, prepared_pod)) = loaded else { continue }; // unchanged since last scan

            let mirror = build_mirror_pod(&prepared_pod, &cfg.node_name);
            let mirror_name = mirror.metadata.name.clone().unwrap_or_default();
            let namespace = mirror.metadata.namespace.clone().unwrap_or_else(|| "default".to_string());
            if mirror_name.is_empty() {
                warn!(path = %path.display(), "static pod: manifest has no metadata.name; skipping");
                continue;
            }

            let api: Api<Pod> = Api::namespaced(client.clone(), &namespace);
            let pp = PatchParams::apply("nodelet-static-pod").force();
            if let Err(e) = api.patch(&mirror_name, &pp, &Patch::Apply(&mirror)).await {
                warn!(pod = %mirror_name, error = ?e, "static pod: failed to create/update mirror pod");
            }

            match runtime.ensure_pod(&prepared_pod).await {
                Ok(status) => {
                    let prev = api.get_opt(&mirror_name).await.ok().flatten().and_then(|p| p.status);
                    if let Err(e) =
                        crate::pods::write_status(&client, &host_ip, &namespace, &mirror_name, &status, prev.as_ref(), &health).await
                    {
                        warn!(pod = %mirror_name, error = ?e, "static pod: failed to write mirror pod status");
                    }
                }
                Err(e) => warn!(pod = %mirror_name, error = ?e, "static pod: ensure_pod failed"),
            }

            tracked.insert(path.clone(), TrackedManifest { hash, prepared_pod, mirror_name, namespace });
        }

        // Manifests that vanished since the last scan: tear the pod and its mirror down.
        let gone: Vec<PathBuf> = tracked.keys().filter(|p| !seen.contains(*p)).cloned().collect();
        for path in gone {
            let Some(entry) = tracked.remove(&path) else { continue };
            if let Err(e) = runtime.remove_pod(&entry.prepared_pod).await {
                warn!(pod = %entry.mirror_name, error = ?e, "static pod: remove_pod failed");
            }
            let api: Api<Pod> = Api::namespaced(client.clone(), &entry.namespace);
            if let Err(e) = api.delete(&entry.mirror_name, &kube::api::DeleteParams::default()).await {
                warn!(pod = %entry.mirror_name, error = ?e, "static pod: failed to delete mirror pod");
            }
        }
    }
}

#[cfg(test)]
#[path = "static_pods_tests/scan_manifest_dir.rs"]
mod tests_scan_manifest_dir;
#[cfg(test)]
#[path = "static_pods_tests/parse_manifest.rs"]
mod tests_parse_manifest;
#[cfg(test)]
#[path = "static_pods_tests/mirror_pod.rs"]
mod tests_mirror_pod;
#[cfg(test)]
#[path = "static_pods_tests/load_if_changed.rs"]
mod tests_load_if_changed;
