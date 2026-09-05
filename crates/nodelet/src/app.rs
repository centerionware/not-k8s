//! The node agent itself — everything `nodelet`'s `main()` used to be.
//!
//! Pairs with a stripped real control plane (`k3s server --disable-agent`) so the
//! device speaks 1:1 kubectl/CRD Kubernetes while shedding the kubelet's idle cost
//! (PLEG polling, cAdvisor housekeeping). See docs/ARCHITECTURE.md.
//!
//! This lives in the library rather than in `main.rs` so the combined
//! `notk8s` binary (crates/notk8s) can run the identical code path without
//! a second executable re-linking the shared dependency tree. `main.rs` is
//! now a wrapper around [`run`], and nothing about the agent's behaviour
//! depends on which of the two binaries called it.

use crate::config::{Config, RuntimeKind};
use crate::runtime::{self, PodRuntime};
use crate::{node, pods};
use anyhow::{Context, Result};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::{error, info, warn};

/// Install rustls' default CryptoProvider, unless something already did.
///
/// rustls 0.23 stopped silently picking one — with more than one call site
/// able to reach for one lazily (kube's client here, plus whatever CRI's
/// tonic pulls in under --features cri), it needs installing explicitly
/// before anything opens a TLS connection. Without it,
/// `kube::Client::try_default()` panics on *every* startup — this crashed
/// nodelet unconditionally in practice, just invisibly, since nothing was
/// watching it restart until it ran as a real service instead of a bare
/// backgrounded process.
///
/// `install_default()` errors on a second call, which a single-purpose
/// binary can treat as impossible but the combined binary cannot — hence
/// the check rather than an unconditional `expect()`.
pub fn install_crypto_provider() {
    if rustls::crypto::CryptoProvider::get_default().is_none() {
        rustls::crypto::ring::default_provider()
            .install_default()
            .expect("installing default rustls CryptoProvider (no other provider was installed a moment ago)");
    }
}

/// Run the node agent. Never returns under normal operation.
pub async fn run() -> Result<()> {
    install_crypto_provider();

    let cfg = Config::from_env().context("loading configuration")?;
    info!(
        node = %cfg.node_name,
        runtime = ?cfg.runtime,
        cpu = cfg.cpu_cores,
        memory_bytes = cfg.memory_bytes,
        max_pods = cfg.max_pods,
        "starting nodelet"
    );

    // TLS bootstrap: no-op unless NODELET_BOOTSTRAP_KUBECONFIG is set. On
    // success this sets $KUBECONFIG to the freshly issued client-cert
    // kubeconfig before the real client below is built.
    nodelet::bootstrap::run(&cfg).await.context("TLS bootstrap")?;

    let client = kube::Client::try_default()
        .await
        .context("building kube client (is KUBECONFIG set and the apiserver reachable?)")?;

    // Pick the runtime. Mock needs nothing; CRI needs the `cri` feature + containerd
    // (and the kube Client, to resolve ConfigMap/Secret volumes — CRI itself has
    // no concept of those, only host-path bind mounts).
    info!("initializing pod runtime");
    let runtime: Arc<dyn PodRuntime> = build_runtime(&cfg, client.clone()).await?;
    info!("pod runtime initialized");

    // Register the node and seed status + lease before we start reconciling pods.
    let images = match tokio::time::timeout(Duration::from_secs(15), runtime.node_images()).await {
        Ok(result) => result.unwrap_or_else(|e| {
            warn!(error = ?e, "CRI image inventory failed during node startup; continuing with an empty image list");
            Vec::new()
        }),
        Err(_) => {
            warn!("CRI image inventory timed out during node startup; continuing with an empty image list");
            Vec::new()
        }
    };
    let runtime_handlers = match tokio::time::timeout(Duration::from_secs(15), runtime.runtime_handlers()).await {
        Ok(result) => result.unwrap_or_else(|e| {
            warn!(error = ?e, "CRI runtime-handler query failed during node startup; continuing with no runtime handlers");
            Vec::new()
        }),
        Err(_) => {
            warn!("CRI runtime-handler query timed out during node startup; continuing with no runtime handlers");
            Vec::new()
        }
    };
    info!(image_count = images.len(), runtime_handler_count = runtime_handlers.len(), "registering node");
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

    // PodResources API: kubelet's own gRPC service (List/GetAllocatableResources/
    // Get) for external device-monitoring tooling. No-op unless
    // NODELET_POD_RESOURCES_SOCKET_PATH is non-empty (has a real default).
    #[cfg(feature = "cri")]
    tokio::spawn(nodelet::pod_resources::run(runtime.clone(), cfg.clone()));

    // ClusterIP/NodePort routing is NOT here — it's the `nodeproxy` binary
    // (crates/nodeproxy), the same way kube-proxy is a separate component
    // from the kubelet upstream. nodelet neither starts nor depends on it.

    // The pod control loop. watcher() self-heals on watch errors; we only loop if
    // the stream fully terminates.
    //
    // "Self-heals on watch errors" is true of a watch that was *interrupted*
    // and misleading about one that cannot **start**: there, every poll fails
    // immediately and forever, and this loop would never see the stream
    // terminate because it never does. That is not a hypothetical — taken
    // literally, this comment is what left the watches in pods.rs unpaced
    // until they were measured busy-looping at 128 requests a second against
    // a restarting apiserver. The pacing for that case lives on the streams
    // themselves (pods.rs's WatchBackoffPolicy), not here.
    let mut controller = pods::PodController::new_with_dns_gate(
        client,
        runtime,
        cfg.node_name.clone(),
        !cfg.cluster_dns.is_empty(),
    );
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
                info!(endpoint = %cfg.cri_endpoint, "connecting to CRI endpoint");
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
                info!(endpoint = %cfg.cri_endpoint, "connected to CRI endpoint");
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

/// Terminate a pod for a kubelet-initiated reason (node/pod resource
/// eviction, `activeDeadlineSeconds` exceeded): stop its containers
/// immediately, mark it `Failed` with the given reason, and — critically —
/// do NOT delete the pod object.
///
/// Real kubelet never deletes a pod it terminates this way; it's not
/// kubelet's job (the well-known "evicted pods pile up and need a manual
/// `kubectl delete`" behavior everyone's run into) — the object is left
/// for the owning controller (a ReplicaSet noticing Failed and creating a
/// replacement) or the apiserver's own terminated-pod GC (a real
/// kube-controller-manager component, part of this project's kept-real
/// control plane) to eventually clean up. This previously deleted the pod
/// with `grace_period_seconds: 0` immediately after patching status —
/// found live (round 123) that this isn't just a upstream-semantics
/// deviation but a genuine race: an essentially-instant hard delete can
/// remove the object before *any* observer (including this project's own
/// e2e tests) ever gets to see the `Evicted` status at all.
///
/// Needs `pods.rs::PodController::reconcile()`'s own terminal-phase check
/// to be safe: once this stops containers via `remove_pod()` without
/// deleting the object, the status patch below generates a fresh watch
/// event, and without that check a `restartPolicy: Always` pod's next
/// reconcile would see "no containers running" and recreate them —
/// exactly the scenario real kubernetes' "a pod's phase never leaves
/// Failed/Succeeded once it gets there" rule exists to prevent.
async fn evict_pod(client: &kube::Client, runtime: &Arc<dyn PodRuntime>, pod: &k8s_openapi::api::core::v1::Pod, status_reason: &str, message: &str) {
    use kube::api::{Api, Patch, PatchParams};
    let (Some(ns), Some(name)) = (pod.metadata.namespace.as_deref(), pod.metadata.name.as_deref()) else { return };

    let pod_api: Api<k8s_openapi::api::core::v1::Pod> = Api::namespaced(client.clone(), ns);
    let status_patch = serde_json::json!({
        "status": { "phase": "Failed", "reason": status_reason, "message": message }
    });
    // Mark the Pod terminal before stopping its sandbox. The stop operation
    // emits runtime/watch events, and a stale event already queued in the
    // controller can otherwise recreate the containers in the gap between
    // remove_pod() and this status patch. That race was observed live with
    // activeDeadlineSeconds: the stale reconcile created a new sandbox,
    // changed podIPs, and the Pod never stayed Failed.
    let status_patched = match pod_api.patch_status(name, &PatchParams::default(), &Patch::Merge(&status_patch)).await {
        Ok(_) => true,
        Err(e) => {
            warn!(pod = %format!("{ns}/{name}"), error = ?e, "eviction: failed to patch pod status");
            false
        }
    };

    if let Err(e) = runtime.remove_pod(pod).await {
        warn!(pod = %format!("{ns}/{name}"), error = ?e, "eviction: failed to stop containers");
    }

    // A reconcile that was already inside ensure_pod() when the first patch
    // landed can finish afterward and write a fresh non-terminal status. The
    // runtime stop emits its own events too, so repeat the terminal patch
    // after stopping: the last writer must be the eviction path, otherwise
    // activeDeadlineSeconds can be lost and the Pod keeps running forever.
    if status_patched {
        if let Err(e) = pod_api.patch_status(name, &PatchParams::default(), &Patch::Merge(&status_patch)).await {
            warn!(pod = %format!("{ns}/{name}"), error = ?e, "eviction: failed to reaffirm pod status");
        }
    }

    if status_patched {
        info!(pod = %format!("{ns}/{name}"), status_reason, "evicted pod (containers stopped; object left for cleanup, matching real kubelet)");
    }
}

/// Re-check node pressure on `cfg.eviction_check_interval` and, if
/// MemoryPressure or DiskPressure is active, evict exactly one eligible pod
/// (see `nodelet::eviction`) — never a mass cull; the next tick re-measures
/// and decides again, same as real kubelet's eviction manager. A *hard*
/// threshold acts the same tick it's crossed; a *soft* one (round 101)
/// first needs to stay continuously crossed for `cfg.eviction_soft_grace_period`
/// (`soft_since`, tracked across ticks) — real kubelet's own
/// eviction-soft/-soft-grace-period pair, this codebase's prior "hard-style
/// immediate action only" simplification.
async fn eviction_loop(client: kube::Client, runtime: Arc<dyn PodRuntime>, cfg: Config) {
    use k8s_openapi::api::core::v1::Pod;
    use kube::api::{Api, ListParams};
    use std::collections::HashMap;

    // Round 101: how long each resource has been continuously past its
    // *soft* (but not yet hard) threshold — real kubelet's
    // eviction-soft-grace-period bookkeeping. Cleared whenever that
    // resource drops back under its soft threshold; `Instant`, not
    // wall-clock, since only elapsed-since-start-of-tracking matters and
    // this never survives a restart (matching the rest of eviction_loop's
    // own no-persisted-state design).
    let mut soft_since: HashMap<&'static str, tokio::time::Instant> = HashMap::new();

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

        // activeDeadlineSeconds is independent of resource-pressure ranking.
        // Check it before the CRI stats RPC: a slow or wedged
        // ListPodSandboxStats call must not delay a deadline eviction past
        // the pod's own deadline.
        let now = k8s_openapi::jiff::Timestamp::now();
        let over_active_deadline = pods.iter().find(|p| {
            if p.metadata.deletion_timestamp.is_some() {
                return false;
            }
            let active_deadline_seconds = p.spec.as_ref().and_then(|s| s.active_deadline_seconds).map(i64::from);
            let seconds_since_start =
                p.status.as_ref().and_then(|s| s.start_time.as_ref()).map(|t| (now - t.0).get_seconds());
            nodelet::eviction::active_deadline_exceeded(active_deadline_seconds, seconds_since_start)
        });
        if let Some(victim) = over_active_deadline {
            evict_pod(&client, &runtime, victim, "DeadlineExceeded", "Pod was active on the node longer than the specified deadline").await;
            continue;
        }

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
            evict_pod(&client, &runtime, victim, "Evicted", "The node was low on resource: Ephemeral storage limit exceeded.").await;
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
            evict_pod(&client, &runtime, victim, "Evicted", &format!("The node was low on resource: emptyDir volume '{volume}' exceeded its sizeLimit.")).await;
            continue; // one pod per check, matching the pressure-based path below
        }

        let hard = nodelet::metrics::read_pressure(
            &cfg.disk_path,
            cfg.memory_pressure_threshold_bytes,
            cfg.disk_pressure_percent,
            cfg.pid_pressure_percent,
        );
        // Round 101: the looser eviction-soft signals, using the same live
        // reads as `hard` above but against cfg's soft thresholds (which
        // default equal to the hard ones, making this a no-op by
        // construction for any deployment that hasn't set them).
        let soft = nodelet::metrics::read_pressure(
            &cfg.disk_path,
            cfg.memory_pressure_soft_threshold_bytes,
            cfg.disk_pressure_soft_percent,
            cfg.pid_pressure_soft_percent,
        );
        let now = tokio::time::Instant::now();
        let mut track = |key: &'static str, soft_true: bool| -> Option<std::time::Duration> {
            if !soft_true {
                soft_since.remove(key);
                return None;
            }
            let since = *soft_since.entry(key).or_insert(now);
            Some(now.duration_since(since))
        };
        let memory_elapsed = track("memory", soft.memory);
        let disk_elapsed = track("disk", soft.disk);
        let pid_elapsed = track("pid", soft.pid);
        drop(track);

        let memory_due = nodelet::eviction::pressure_action_due(hard.memory, memory_elapsed, cfg.eviction_soft_grace_period);
        let disk_due = nodelet::eviction::pressure_action_due(hard.disk, disk_elapsed, cfg.eviction_soft_grace_period);
        let pid_due = nodelet::eviction::pressure_action_due(hard.pid, pid_elapsed, cfg.eviction_soft_grace_period);
        if !memory_due && !disk_due && !pid_due {
            continue;
        }
        let reason = if memory_due {
            "MemoryPressure"
        } else if disk_due {
            "DiskPressure"
        } else {
            "PIDPressure"
        };

        let Some(victim) = nodelet::eviction::pick_eviction_candidate(&pods, &usage_bytes_by_uid) else {
            continue; // under pressure, but nothing eligible to evict
        };
        evict_pod(&client, &runtime, victim, "Evicted", &format!("The node was low on resource: {reason}.")).await;
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
