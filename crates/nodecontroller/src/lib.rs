//! nodecontroller — kube-controller-manager's job, done event-driven where
//! upstream is, and honestly polled (not pretended away) where it isn't.
//!
//! Read `docs/CONTROLLER_MANAGER.md` first: it's the scope (every group,
//! upstream's `NewControllerDescriptors()` as source of truth, not the
//! reference docs page) and the polling architecture (`wheel.rs` +
//! `pacing.rs`) this crate is built around. Group A (node lifecycle),
//! Group B (service routing, `endpointslice-controller`), the object-count
//! slice of Group D (`resourcequota-controller`), and all four workload
//! controllers of Group E (`replicaset-controller`/`deployment-controller`/
//! `daemonset-controller`/`statefulset-controller`) are implemented, plus
//! the minimum slice of Group C (`serviceaccount-controller` only) needed
//! to unblock testing Group A at all, plus `garbage-collector-controller`
//! (Group D's other half, deferred until Group E existed to produce a real
//! owner chain to clean up) — see `controllers/service_account.rs`'s,
//! `controllers/resource_quota.rs`'s, `controllers/garbage_collector.rs`'s,
//! and each `controllers/*.rs` workload file's own module doc for why each
//! is scoped the way it is. Group F (batch controllers —
//! `job-controller`/`cronjob-controller`/`ttl-after-finished-controller`)
//! is implemented too, see `controllers/job.rs`, `controllers/cron_job.rs`,
//! `controllers/ttl_after_finished.rs`, and `cron_schedule.rs`. All of
//! Group G is implemented: `attach-detach-controller`
//! (`controllers/attach_detach.rs`, Tier 0), `persistentvolume-binder-controller`
//! (`controllers/pv_binder.rs`), and pv/pvc-protection-controller
//! (`controllers/storage_protection.rs`) — `persistentvolume-expander-controller`
//! is scoped out (see `docs/CONTROLLER_MANAGER.md`'s Group G section: no
//! in-tree volume plugins exist to expand, and CSI resize goes through the
//! external-resizer sidecar directly against the PVC, no controller-manager
//! involvement needed). `root-ca-cert-publisher-controller`
//! (`controllers/root_ca_publisher.rs`, Group C) was pulled forward out of
//! delivery order the same way `serviceaccount-controller` was — confirmed
//! live in CI while verifying Group G that its absence silently breaks any
//! Pod-side component (the CSI external-provisioner sidecar, concretely)
//! that builds its own in-cluster client, since `kube-root-ca.crt` never
//! existed in any namespace for its projected token volume to mount.
//! **Group G's dynamic-CSI e2e path is a known, unresolved regression** —
//! see `docs/CONTROLLER_MANAGER.md`'s Group G section and
//! `pv_binder.rs`'s module doc for the diagnostic trail; this blocks
//! merging the whole multi-group PR and must be resolved, not silently
//! left. Group H's `ephemeral-volume-controller`/`resourceclaim-controller`
//! (`controllers/resource_claim.rs`) is implemented, pairing with
//! nodelet's existing DRA consumer side in `runtime/cri/claims.rs`;
//! `device-taint-eviction-controller` is scoped out (no infrastructure in
//! this project's e2e suite to verify it against). Group I
//! (`controllers/csr.rs`: `certificatesigningrequest-{approving,signing,cleaner}-controller`)
//! is implemented too — the one place in this crate needing the cluster
//! CA's private key, not just its cert; see that file's own module doc
//! for the config knobs and the degraded-but-not-crashed behavior when the
//! key isn't available. `bootstrap-signer-controller`/`token-cleaner-controller`
//! remain deferred (legacy kubeadm bootstrap-token flow). Group J's
//! `disruption-controller` (`controllers/disruption.rs`) is implemented,
//! keeping `PodDisruptionBudget.status` current for the apiserver's own
//! eviction-admission check to read; `horizontalpodautoscaler-controller`
//! is scoped out (no metrics-server in this project's e2e infrastructure
//! to verify a real scaling decision against — see
//! `docs/CONTROLLER_MANAGER.md`'s Group J section). With Group J's
//! verifiable half done, **every group through J is now implemented**
//! except the one open item Group G's own section documents (the
//! dynamic-CSI e2e path, a real regression blocking merge) — see that
//! section and `pv_binder.rs`'s module doc before assuming this crate is
//! ready to ship.
//!
//! Single leader-election lease (`kube-system/kube-controller-manager`,
//! matching upstream's own name — see `config.rs`) covers the whole
//! process, the same as upstream elects once rather than per-controller:
//! every controller in this crate is a single writer of its own objects,
//! and standing that up twice is a race, not redundancy — the same
//! reasoning `nodescheduler`'s leader election documents for `Binding`.

pub mod config;
pub mod controllers;
pub mod cron_schedule;
pub mod jitter;
pub mod k8s_time;
pub mod pacing;
pub mod watch;
pub mod wheel;

use anyhow::{Context, Result};
use std::future::Future;

/// Start one controller unless its diagnostic switch is set.
///
/// Request pacing belongs at the shared client boundary, not here. Sleeping
/// each controller by a fixed slot only spaces the first poll of each future;
/// it does not bound the LISTs, watches, or reconcile writes that follow.
async fn start_controller<F>(cfg: &config::Config, name: &'static str, controller: F) -> Result<()>
where
    F: Future<Output = Result<()>>,
{
    if cfg.controller_disabled(name) {
        tracing::info!(controller = name, "nodecontroller controller disabled by configuration");
        return Ok(());
    }
    tracing::debug!(controller = name, "starting nodecontroller controller");
    controller.await
}

/// Install rustls' default `CryptoProvider`, unless something already did.
///
/// rustls 0.23 stopped silently picking one, and `kube::Client::try_default()`
/// panics rather than erroring without it — confirmed live in CI (e2e.yml
/// run 31875853444): `nodecontroller.service` crash-looped on exactly this
/// panic, since this crate was the one place that copy-pasting
/// `nodescheduler`'s/`nodeproxy`'s/`nodelet`'s own `install_crypto_provider()`
/// got missed. `install_default()` itself errors on a second call, which a
/// standalone binary can treat as impossible but the combined `notk8s`
/// binary cannot (every component's `run()` could be reached in-process) —
/// hence the check rather than an `expect()` alone.
fn install_crypto_provider() {
    if rustls::crypto::CryptoProvider::get_default().is_none() {
        rustls::crypto::ring::default_provider()
            .install_default()
            .expect("installing default rustls CryptoProvider (no other provider was installed a moment ago)");
    }
}

pub async fn run() -> Result<()> {
    install_crypto_provider();

    let cfg = config::Config::from_env()?;
    // All controllers, leader election, shared informers, and reconcile
    // writes use this one client. A single async rate gate at this boundary
    // prevents a controller startup or event fan-out from creating a burst
    // of HTTP requests. The limit is on request starts, not on Tokio worker
    // time: waiting is a pending future, and a response body (including a
    // long-running watch) is never held by a CPU-budget permit.
    let kube_config = kube::Config::infer().await.context("loading apiserver configuration")?;
    let election_client = kube::client::ClientBuilder::try_from(kube_config.clone())
        .context("building apiserver client")?
        .build();
    let client = kube::client::ClientBuilder::try_from(kube_config)
        .context("building rate-limited controller apiserver client")?
        .with_layer(&tower::limit::RateLimitLayer::new(1, cfg.api_request_interval))
        .build();

    let election_cfg = cfg.election();
    node_leaderelection::run_as_leader(election_client, &election_cfg, || async move {
        tracing::info!("nodecontroller is now leading — starting all controllers");
        tokio::try_join!(
            start_controller(&cfg, "node-ipam", controllers::node_ipam::run(client.clone(), &cfg)),
            start_controller(&cfg, "node-lifecycle", controllers::node_lifecycle::run(client.clone(), &cfg)),
            start_controller(&cfg, "service-account", controllers::service_account::run(client.clone(), &cfg)),
            start_controller(&cfg, "endpoint-slice", controllers::endpoint_slice::run(client.clone(), &cfg)),
            start_controller(&cfg, "resource-quota", controllers::resource_quota::run(client.clone(), &cfg)),
            start_controller(&cfg, "replica-set", controllers::replica_set::run(client.clone(), &cfg)),
            start_controller(&cfg, "deployment", controllers::deployment::run(client.clone(), &cfg)),
            start_controller(&cfg, "daemon-set", controllers::daemon_set::run(client.clone(), &cfg)),
            start_controller(&cfg, "stateful-set", controllers::stateful_set::run(client.clone(), &cfg)),
            start_controller(&cfg, "garbage-collector", controllers::garbage_collector::run(client.clone(), &cfg)),
            start_controller(&cfg, "job", controllers::job::run(client.clone(), &cfg)),
            start_controller(&cfg, "cron-job", controllers::cron_job::run(client.clone(), &cfg)),
            start_controller(&cfg, "ttl-after-finished", controllers::ttl_after_finished::run(client.clone(), &cfg)),
            start_controller(&cfg, "attach-detach", controllers::attach_detach::run(client.clone(), &cfg)),
            start_controller(&cfg, "pv-binder", controllers::pv_binder::run(client.clone(), &cfg)),
            start_controller(&cfg, "storage-protection", controllers::storage_protection::run(client.clone(), &cfg)),
            start_controller(&cfg, "root-ca-publisher", controllers::root_ca_publisher::run(client.clone(), &cfg)),
            start_controller(&cfg, "resource-claim", controllers::resource_claim::run(client.clone(), &cfg)),
            start_controller(&cfg, "csr", controllers::csr::run(client.clone(), &cfg)),
            start_controller(&cfg, "disruption", controllers::disruption::run(client.clone(), &cfg)),
        )?;
        Ok(())
    })
    .await
}
