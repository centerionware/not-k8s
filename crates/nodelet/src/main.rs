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
    node::register(&client, &cfg)
        .await
        .context("registering node with the apiserver")?;
    info!(node = %cfg.node_name, "node registered and Ready");

    // Cheap, frequent liveness (Lease) decoupled from infrequent full status push.
    tokio::spawn(heartbeat_loop(client.clone(), cfg.clone()));

    // Coarse periodic housekeeping (orphaned sandboxes, unreferenced
    // images) — a no-op on the mock runtime, see PodRuntime::gc()'s default.
    tokio::spawn(gc_loop(client.clone(), runtime.clone(), cfg.clone()));

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
                let rt = runtime::cri::CriRuntime::connect(
                    &cfg.cri_endpoint,
                    client,
                    cfg.cluster_dns.clone(),
                    cfg.cluster_domain.clone(),
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

/// Renew the Lease every `heartbeat`; push full node status every `status_interval`.
async fn heartbeat_loop(client: kube::Client, cfg: Config) {
    let mut last_status = Instant::now();
    loop {
        tokio::time::sleep(cfg.heartbeat).await;

        if let Err(e) = node::renew_lease(&client, &cfg).await {
            warn!(error = ?e, "lease renewal failed");
        }

        if last_status.elapsed() >= cfg.status_interval {
            match node::push_status(&client, &cfg, true).await {
                Ok(()) => info!("node status pushed"),
                Err(e) => warn!(error = ?e, "node status push failed"),
            }
            last_status = Instant::now();
        }
    }
}
