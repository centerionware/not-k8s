//! nodelet — a lean, event-driven Kubernetes node agent for single-device / edge use.
//!
//! Pairs with a stripped real control plane (`k3s server --disable-agent`) so the
//! device speaks 1:1 kubectl/CRD Kubernetes while shedding the kubelet's idle cost
//! (PLEG polling, cAdvisor housekeeping). See docs/ARCHITECTURE.md.

use anyhow::{Context, Result};
use nodelet::config::{Config, RuntimeKind};
use nodelet::runtime::{self, PodRuntime};
use nodelet::{node, pods};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::{error, info, warn};
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

#[tokio::main]
async fn main() -> Result<()> {
    // rustls 0.23 stopped silently picking a default CryptoProvider — with
    // more than one call site able to reach for one lazily (kube's client
    // here, plus whatever CRI's tonic pulls in under --features cri), it
    // needs installing explicitly exactly once, before anything opens a TLS
    // connection. Without this, kube::Client::try_default() panics on
    // *every* startup — this crashed nodelet unconditionally in practice,
    // just invisibly, since nothing was watching it restart until it ran as
    // a real service instead of a bare backgrounded process.
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("installing default rustls CryptoProvider (should only fail if called twice)");

    tracing_subscriber::registry()
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .with(fmt::layer().with_target(false))
        .init();

    let cfg = Config::from_env().context("loading configuration")?;
    info!(
        node = %cfg.node_name,
        runtime = ?cfg.runtime,
        cpu = cfg.cpu_cores,
        memory_bytes = cfg.memory_bytes,
        max_pods = cfg.max_pods,
        "starting nodelet"
    );

    let client = kube::Client::try_default()
        .await
        .context("building kube client (is KUBECONFIG set and the apiserver reachable?)")?;

    // Pick the runtime. Mock needs nothing; CRI needs the `cri` feature + containerd
    // (and the kube Client, to resolve ConfigMap/Secret volumes — CRI itself has
    // no concept of those, only host-path bind mounts).
    let runtime: Arc<dyn PodRuntime> = build_runtime(&cfg, client.clone()).await?;

    // Register the node and seed status + lease before we start reconciling pods.
    let images = runtime.node_images().await.unwrap_or_default();
    let runtime_handlers = runtime.runtime_handlers().await.unwrap_or_default();
    node::register(
        &client,
        &cfg,
        &runtime.device_plugin_capacity(),
        images,
        &runtime.mounted_csi_volumes(),
        &runtime_handlers,
    )
    .await
    .context("registering node with the apiserver")?;
    info!(node = %cfg.node_name, "node registered and Ready");

    // Node allocatable enforcement: caps the top-level "kubepods" cgroup at
    // Node.status.allocatable (capacity minus system/kube-reserved) so pods
    // collectively can never exceed it. Best-effort (needs root + cgroup
    // v2) — see cgroup.rs for why a failure here doesn't block startup.
    #[cfg(feature = "cri")]
    if matches!(cfg.runtime, RuntimeKind::Cri) {
        let allocatable_cpu_millicores = (cfg.cpu_cores * 1000)
            .saturating_sub(cfg.system_reserved_cpu_millicores + cfg.kube_reserved_cpu_millicores);
        let allocatable_memory_bytes =
            cfg.memory_bytes.saturating_sub(cfg.system_reserved_memory_bytes + cfg.kube_reserved_memory_bytes);
        nodelet::cgroup::enforce_node_allocatable(&cfg.cgroup_fs_root, allocatable_cpu_millicores, allocatable_memory_bytes);
    }

    // Cheap, frequent liveness (Lease) decoupled from infrequent full status push.
    tokio::spawn(heartbeat_loop(client.clone(), cfg.clone(), runtime.clone()));

    // Coarse periodic housekeeping (orphaned sandboxes, unreferenced
    // images) — a no-op on the mock runtime, see PodRuntime::gc()'s default.
    tokio::spawn(gc_loop(client.clone(), runtime.clone(), cfg.clone()));

    // Node-pressure eviction: re-checks real MemoryPressure/DiskPressure
    // (see metrics.rs) on its own short interval and reclaims resources by
    // evicting one eligible pod at a time when either is active.
    tokio::spawn(eviction_loop(client.clone(), runtime.clone(), cfg.clone()));

    // Container log rotation — a no-op on the mock runtime, see
    // PodRuntime::rotate_logs()'s default.
    tokio::spawn(log_rotate_loop(runtime.clone(), cfg.clone()));

    // Static pods: no-op unless NODELET_STATIC_POD_PATH is set.
    tokio::spawn(nodelet::static_pods::run(client.clone(), runtime.clone(), cfg.clone()));

    // kubelet-style HTTP(S) server: containerLogs/exec/attach/portForward.
    // No-op unless NODELET_SERVER_ENABLED (defaults on for the cri runtime).
    #[cfg(feature = "cri")]
    tokio::spawn(nodelet::server::run(client.clone(), runtime.clone(), cfg.clone()));

    // Graceful node shutdown: no-op unless NODELET_SHUTDOWN_GRACE_PERIOD_SECS
    // is set (systemd-logind inhibitor lock + pod drain on shutdown).
    #[cfg(feature = "cri")]
    tokio::spawn(nodelet::shutdown::run(client.clone(), runtime.clone(), cfg.clone()));

    // ClusterIP/NodePort routing (nftables). No-op if disabled or if `nft`
    // isn't usable — pods and direct pod-IP traffic work either way.
    if cfg.service_proxy {
        let svc_client = client.clone();
        let (ip_family, lb_method) = (cfg.ip_family, cfg.lb_method);
        tokio::spawn(async move {
            nodelet::svc::ServiceProxy::new(svc_client, ip_family, lb_method).run().await
        });
    }

    // The pod control loop. watcher() self-heals on watch errors; we only loop if
    // the stream fully terminates.
    let mut controller = pods::PodController::new(client, runtime, cfg.node_name.clone());
    loop {
        if let Err(e) = controller.run().await {
            error!(error = ?e, "pod controller exited with error");
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

async fn build_runtime(cfg: &Config, #[allow(unused_variables)] client: kube::Client) -> Result<Arc<dyn PodRuntime>> {
    match cfg.runtime {
        RuntimeKind::Mock => {
            info!("using mock runtime (no container engine; reports pods Running)");
            Ok(Arc::new(runtime::mock::MockRuntime::new()))
        }
        RuntimeKind::Cri => {
            #[cfg(feature = "cri")]
            {
                info!(endpoint = %cfg.cri_endpoint, "using CRI runtime (containerd)");
                let cpu_manager = cfg.cpu_manager_static.then(|| {
                    nodelet::cpu_manager::CpuManager::new(
                        cfg.cpu_cores,
                        cfg.system_reserved_cpu_millicores + cfg.kube_reserved_cpu_millicores,
                    )
                });
                let topology_policy = match cfg.topology_manager_policy.as_str() {
                    "best-effort" => nodelet::topology::TopologyManagerPolicy::BestEffort,
                    "restricted" => nodelet::topology::TopologyManagerPolicy::Restricted,
                    "single-numa-node" => nodelet::topology::TopologyManagerPolicy::SingleNumaNode,
                    _ => nodelet::topology::TopologyManagerPolicy::None,
                };
                let numa_topology = nodelet::topology::read_numa_topology(std::path::Path::new("/sys/devices/system/node"));
                let memory_manager = cfg
                    .memory_manager_static
                    .then(|| nodelet::memory_manager::MemoryManager::new(nodelet::topology::read_numa_memory(std::path::Path::new("/sys/devices/system/node"))));
                let userns = nodelet::userns::UsernsAllocator::new(cfg.userns_base_uid, cfg.userns_length, cfg.userns_max_pods);
                let rt = runtime::cri::CriRuntime::connect(
                    &cfg.cri_endpoint,
                    client,
                    cfg.node_name.clone(),
                    cfg.cluster_dns.clone(),
                    cfg.cluster_domain.clone(),
                    cfg.csi_drivers.clone(),
                    cfg.plugin_registry_path.clone(),
                    cfg.plugin_registry_sync_interval,
                    cpu_manager,
                    memory_manager,
                    topology_policy,
                    numa_topology,
                    userns,
                    cfg.memory_bytes as i64,
                    (cfg.cpu_cores * 1000) as i64,
                    cfg.memory_swap_bytes as i64,
                    cfg.memory_swap_limited,
                    cfg.disk_path.clone(),
                    cfg.image_gc_high_threshold_percent,
                    cfg.image_gc_low_threshold_percent,
                    cfg.image_gc_min_age_secs,
                    cfg.image_credential_provider_config.clone(),
                    cfg.image_credential_provider_bin_dir.clone(),
                )
                .await
                .context("connecting to CRI endpoint")?;
                Ok(Arc::new(rt))
            }
            #[cfg(not(feature = "cri"))]
            {
                let _ = cfg;
                anyhow::bail!(
                    "NODELET_RUNTIME=cri requires building with `--features cri` (containerd support)"
                )
            }
        }
    }
}

/// Periodically list Pods bound to this node and hand the runtime the set
/// of keys still live, so it can remove anything it has that the apiserver
/// doesn't (see `gc.rs`).
async fn gc_loop(client: kube::Client, runtime: Arc<dyn PodRuntime>, cfg: Config) {
    use k8s_openapi::api::core::v1::Pod;
    use kube::api::{Api, ListParams};

    loop {
        tokio::time::sleep(cfg.gc_interval).await;

        let api: Api<Pod> = Api::all(client.clone());
        let params = ListParams::default().fields(&format!("spec.nodeName={}", cfg.node_name));
        let live_pod_keys = match api.list(&params).await {
            Ok(list) => list
                .items
                .iter()
                .filter_map(|p| {
                    let ns = p.metadata.namespace.as_deref().unwrap_or("default");
                    let name = p.metadata.name.as_deref()?;
                    Some(nodelet::runtime::pod_key(ns, name))
                })
                .collect(),
            Err(e) => {
                warn!(error = ?e, "gc: failed to list pods; skipping this cycle");
                continue;
            }
        };

        if let Err(e) = runtime.gc(&live_pod_keys).await {
            warn!(error = ?e, "gc: cycle failed");
        }
    }
}

/// Re-check node pressure on `cfg.eviction_check_interval` and, if
/// MemoryPressure or DiskPressure is active, evict exactly one eligible pod
/// (see `nodelet::eviction`) — never a mass cull; the next tick re-measures
/// and decides again, same as real kubelet's eviction manager.
/// Evict one pod: best-effort `Evicted`/`Failed` status patch (surfacing
/// why, same as real kubelet — but the delete is what actually reclaims
/// anything, so a failed status patch must never block it), then a
/// zero-grace delete.
async fn evict_pod(client: &kube::Client, ns: &str, name: &str, reason: &str) {
    use kube::api::{Api, DeleteParams, Patch, PatchParams};
    let pod_api: Api<k8s_openapi::api::core::v1::Pod> = Api::namespaced(client.clone(), ns);
    let status_patch = serde_json::json!({
        "status": { "phase": "Failed", "reason": "Evicted", "message": format!("The node was low on resource: {reason}.") }
    });
    let _ = pod_api.patch_status(name, &PatchParams::default(), &Patch::Merge(&status_patch)).await;

    let dp = DeleteParams { grace_period_seconds: Some(0), ..Default::default() };
    match pod_api.delete(name, &dp).await {
        Ok(_) => info!(pod = %format!("{ns}/{name}"), reason, "evicted pod"),
        Err(e) => warn!(pod = %format!("{ns}/{name}"), error = ?e, "eviction: failed to delete pod"),
    }
}

async fn eviction_loop(client: kube::Client, runtime: Arc<dyn PodRuntime>, cfg: Config) {
    use k8s_openapi::api::core::v1::Pod;
    use kube::api::{Api, ListParams};
    use std::collections::HashMap;

    loop {
        tokio::time::sleep(cfg.eviction_check_interval).await;

        let api: Api<Pod> = Api::all(client.clone());
        let params = ListParams::default().fields(&format!("spec.nodeName={}", cfg.node_name));
        let pods = match api.list(&params).await {
            Ok(list) => list.items,
            Err(e) => {
                warn!(error = ?e, "eviction: failed to list pods; skipping this cycle");
                continue;
            }
        };

        let usage_stats = match runtime.pod_usage_stats().await {
            Ok(stats) => stats,
            Err(e) => {
                warn!(error = ?e, "eviction: failed to fetch usage stats; ranking by requested memory only this cycle");
                Vec::new()
            }
        };
        // Real usage, keyed by pod UID, for eviction.rs's ranking — falls
        // back to requested memory per-pod when this is empty (mock
        // runtime, or a CRI error listing stats).
        let usage_bytes_by_uid: HashMap<String, u64> = usage_stats
            .iter()
            .filter_map(|u| u.pod.memory_working_set_bytes.or(u.pod.memory_usage_bytes).map(|bytes| (u.uid.clone(), bytes)))
            .collect();

        // Local ephemeral storage (round 49; the deferred half of round
        // 48's arc): a pod exceeding its *own* ephemeral-storage limit is
        // evicted immediately — this is a direct per-pod resource
        // violation (like an OOM kill), independent of general node
        // pressure, so it's checked first and doesn't wait for
        // MemoryPressure/DiskPressure/PIDPressure to be active.
        let ephemeral_usage_by_uid: HashMap<String, u64> =
            usage_stats.iter().filter_map(|u| u.ephemeral_storage_usage_bytes.map(|bytes| (u.uid.clone(), bytes))).collect();
        let over_limit = pods.iter().find(|p| {
            p.metadata.deletion_timestamp.is_none()
                && !nodelet::eviction::is_critical(p)
                && p.metadata
                    .uid
                    .as_deref()
                    .map(|uid| {
                        nodelet::eviction::exceeds_ephemeral_storage_limit(
                            ephemeral_usage_by_uid.get(uid).copied(),
                            nodelet::eviction::ephemeral_storage_limit_bytes(p),
                        )
                    })
                    .unwrap_or(false)
        });
        if let Some(victim) = over_limit {
            if let (Some(ns), Some(name)) = (victim.metadata.namespace.as_deref(), victim.metadata.name.as_deref()) {
                evict_pod(&client, ns, name, "Ephemeral storage limit exceeded").await;
            }
            continue; // one pod per check, matching the pressure-based path below
        }

        // emptyDir.sizeLimit (round 67; found in round 65's fresh gap
        // re-audit): distinct per-volume check from the whole-pod
        // ephemeral-storage limit above — same "direct resource
        // violation, checked ahead of general pressure" reasoning.
        let empty_dir_usage_by_uid: HashMap<String, &HashMap<String, u64>> =
            usage_stats.iter().map(|u| (u.uid.clone(), &u.empty_dir_usage_bytes)).collect();
        let over_empty_dir_limit = pods.iter().find_map(|p| {
            if p.metadata.deletion_timestamp.is_some() || nodelet::eviction::is_critical(p) {
                return None;
            }
            let uid = p.metadata.uid.as_deref()?;
            let usage = empty_dir_usage_by_uid.get(uid)?;
            let limits = nodelet::eviction::empty_dir_size_limits(p);
            let volume = nodelet::eviction::first_empty_dir_over_limit(&limits, usage)?;
            Some((p, volume))
        });
        if let Some((victim, volume)) = over_empty_dir_limit {
            if let (Some(ns), Some(name)) = (victim.metadata.namespace.as_deref(), victim.metadata.name.as_deref()) {
                evict_pod(&client, ns, name, &format!("emptyDir volume '{volume}' exceeded its sizeLimit")).await;
            }
            continue; // one pod per check, matching the pressure-based path below
        }

        let pressure = nodelet::metrics::read_pressure(
            &cfg.disk_path,
            cfg.memory_pressure_threshold_bytes,
            cfg.disk_pressure_percent,
            cfg.pid_pressure_percent,
        );
        if !pressure.memory && !pressure.disk && !pressure.pid {
            continue;
        }
        let reason = if pressure.memory {
            "MemoryPressure"
        } else if pressure.disk {
            "DiskPressure"
        } else {
            "PIDPressure"
        };

        let Some(victim) = nodelet::eviction::pick_eviction_candidate(&pods, &usage_bytes_by_uid) else {
            continue; // under pressure, but nothing eligible to evict
        };
        let (Some(ns), Some(name)) = (victim.metadata.namespace.as_deref(), victim.metadata.name.as_deref()) else {
            continue;
        };
        evict_pod(&client, ns, name, reason).await;
    }
}

/// Rotate any running container's log file once it exceeds
/// `container_log_max_size_bytes`, keeping at most `container_log_max_files`.
async fn log_rotate_loop(runtime: Arc<dyn PodRuntime>, cfg: Config) {
    loop {
        tokio::time::sleep(cfg.log_rotate_interval).await;
        if let Err(e) = runtime.rotate_logs(cfg.container_log_max_size_bytes, cfg.container_log_max_files).await {
            warn!(error = ?e, "log rotation cycle failed");
        }
    }
}

/// Renew the Lease every `heartbeat`; push full node status every `status_interval`.
async fn heartbeat_loop(client: kube::Client, cfg: Config, runtime: Arc<dyn PodRuntime>) {
    let mut last_status = Instant::now();
    loop {
        tokio::time::sleep(cfg.heartbeat).await;

        if let Err(e) = node::renew_lease(&client, &cfg).await {
            warn!(error = ?e, "lease renewal failed");
        }

        if last_status.elapsed() >= cfg.status_interval {
            let images = runtime.node_images().await.unwrap_or_default();
            let runtime_handlers = runtime.runtime_handlers().await.unwrap_or_default();
            match node::push_status(
                &client,
                &cfg,
                true,
                &runtime.device_plugin_capacity(),
                images,
                &runtime.mounted_csi_volumes(),
                &runtime_handlers,
            )
            .await
            {
                Ok(()) => info!("node status pushed"),
                Err(e) => warn!(error = ?e, "node status push failed"),
            }
            last_status = Instant::now();
        }
    }
}
