//! nodecontroller — kube-controller-manager's job, done event-driven where
//! upstream is, and honestly polled (not pretended away) where it isn't.
//!
//! Read `docs/CONTROLLER_MANAGER.md` first: it's the scope (every group,
//! upstream's `NewControllerDescriptors()` as source of truth, not the
//! reference docs page) and the polling architecture (`wheel.rs` +
//! `wheel.rs`) this crate is built around. Group A (node lifecycle),
//! Group B (service routing, `endpointslice-controller`), the object-count
//! slice of Group D (`resourcequota-controller`), and all four workload
//! controllers of Group E (`replicaset-controller`/`deployment-controller`/
//! `daemonset-controller`/`statefulset-controller`) are implemented, plus
//! the minimum slice of Group C (`serviceaccount-controller`) needed
//! to unblock testing Group A at all, plus `namespace-controller` and
//! `garbage-collector-controller`
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
//! **Group G's dynamic-CSI e2e path was diagnosed and fixed before merge**
//! (the PV binder's missing `storage-provisioner` annotation and
//! `bind-completed` handoff — see `docs/CONTROLLER_MANAGER.md`'s Group G
//! section and `pv_binder.rs`'s module doc for the diagnostic trail, and
//! GitHub issue #30 for the closing writeup) — dynamic provisioning,
//! filesystem/raw-block PVCs, and VolumeAttachment are all e2e-verified.
//! Group H's `ephemeral-volume-controller`/`resourceclaim-controller`
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
//! verifiable half done, **every group through J is now implemented** at
//! this crate's documented scope — see `docs/CONTROLLER_MANAGER.md` for
//! the per-group list of what's deliberately deferred (podgc-controller,
//! clusterrole-aggregation-controller, HPA, device-taint-eviction,
//! bootstrap-signer/token-cleaner) before assuming a gap there is a bug.
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
pub mod watch;
pub mod wheel;
pub mod workqueue;

use anyhow::{Context, Result};
use std::future::Future;

/// Start one controller unless its diagnostic switch is set.
///
/// Controllers are started together; informer startup admission and keyed
/// work queues provide the actual backpressure boundaries rather than fixed
/// sleeps or a process-wide request limiter.
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

/// Maps this crate's own controller names to the `system:serviceaccount:
/// kube-system:<name>` identity real upstream `kube-controller-manager`
/// impersonates for that exact controller under `--use-service-account-
/// credentials=true` -- names matching what `kubectl get sa -n kube-system`
/// shows on a real cluster (`pkg/controller/` package names upstream's own
/// bootstrap policy binds `system:controller:<name>` ClusterRoles to, not
/// this crate's own internal naming). See `impersonated_client`'s doc
/// comment for why this exists at all.
fn upstream_controller_sa(name: &str) -> &'static str {
    match name {
        // node-ipam and node-lifecycle both run under upstream's single
        // "node-controller" SA -- they're separate loops here (and
        // separate --controllers entries upstream) but were never split
        // into separate bootstrap identities.
        "node-ipam" | "node-lifecycle" => "node-controller",
        "service-account" => "service-account-controller",
        "namespace" => "namespace-controller",
        "endpoint-slice" => "endpointslice-controller",
        "resource-quota" => "resourcequota-controller",
        "replica-set" => "replicaset-controller",
        "deployment" => "deployment-controller",
        // Confirmed against a real cluster's bootstrap ClusterRoles
        // (docs/E2E_FINDINGS.md finding 22): the real name has a hyphen
        // between "daemon" and "set" that this crate's own controller
        // name doesn't.
        "daemon-set" => "daemon-set-controller",
        "stateful-set" => "statefulset-controller",
        "garbage-collector" => "generic-garbage-collector",
        "job" => "job-controller",
        "cron-job" => "cronjob-controller",
        "ttl-after-finished" => "ttl-after-finished-controller",
        "attach-detach" => "attachdetach-controller",
        "pv-binder" => "persistent-volume-binder",
        // storage-protection covers both PV and PVC protection in one loop
        // here; upstream splits them into two SAs (pv-protection-
        // controller/pvc-protection-controller). Using the PV one -- if
        // this turns out to be missing a PVC-specific verb, that surfaces
        // as an isolated, clearly-attributable 403 on this one controller,
        // not a silent gap.
        "storage-protection" => "pv-protection-controller",
        "root-ca-publisher" => "root-ca-cert-publisher",
        // Same as daemon-set-controller above: real name has a hyphen
        // between "resource" and "claim".
        "resource-claim" => "resource-claim-controller",
        "csr" => "certificate-controller",
        "disruption" => "disruption-controller",
        other => panic!("upstream_controller_sa: no mapping for controller {other:?} -- add one"),
    }
}

/// Builds a client that impersonates `system:serviceaccount:kube-system:
/// <upstream_controller_sa(name)>` -- the same narrowly-scoped identity
/// real upstream `kube-controller-manager` runs this exact controller's
/// **writes** as under `--use-service-account-credentials=true` (which
/// `targets/upstream.rs` sets, expecting this). **Reads go through a
/// different identity entirely** -- see `watch.rs`'s `set_base_client`
/// doc comment: real upstream backs its shared informer factory with the
/// base identity, not per-controller impersonation, and this crate's own
/// `SharedWatch` (`watch.rs`) now matches that split. This client is what
/// each controller's own direct write calls (patch/create/delete) use,
/// and whatever narrow reads its own `system:controller:<name>` bootstrap
/// role happens to include beyond the shared side (e.g. `node-controller`
/// can `get`/`list`/`watch` `nodes`/`pods` directly, confirmed against a
/// real cluster's bootstrap policy).
///
/// Before any of this, every controller shared the single base identity
/// nodecontroller's own kubeconfig authenticates as
/// (`system:kube-controller-manager`) for *everything*, reads and writes
/// alike, which upstream's own bootstrap policy never intended to carry
/// every controller's write permissions -- found live bootstrapping a
/// cluster with `crates/nodebootstrap` (`docs/E2E_FINDINGS.md` finding
/// 22): real 403s creating ConfigMaps. The base identity now only needs
/// the `impersonate` verb on `users` for the write side -- not a broad
/// grant -- since RBAC authorizes every actual request as whichever
/// identity is impersonated, and its own built-in read-heavy role (which
/// backs the shared informer factory, same as upstream) covers reads.
fn impersonated_client(base: &kube::Config, controller_name: &str) -> Result<kube::Client> {
    let mut cfg = base.clone();
    let sa = upstream_controller_sa(controller_name);
    cfg.headers.push((
        http::header::HeaderName::from_static("impersonate-user"),
        http::header::HeaderValue::from_str(&format!("system:serviceaccount:kube-system:{sa}"))
            .with_context(|| format!("building Impersonate-User header for {controller_name}"))?,
    ));
    for group in ["system:serviceaccounts", "system:serviceaccounts:kube-system"] {
        cfg.headers.push((http::header::HeaderName::from_static("impersonate-group"), http::header::HeaderValue::from_static(group)));
    }
    Ok(kube::client::ClientBuilder::try_from(cfg)
        .with_context(|| format!("building impersonated client for controller {controller_name} (sa {sa})"))?
        .build())
}

pub async fn run() -> Result<()> {
    install_crypto_provider();

    let cfg = config::Config::from_env()?;
    watch::configure_startup_concurrency(cfg.watch_startup_concurrency);
    let kube_config = kube::Config::infer().await.context("loading apiserver configuration")?;
    // The base (non-impersonated) identity -- system:kube-controller-
    // manager. Used for election (leader-election is nodecontroller's own
    // concern, not any single controller's) and, as of this fix, for
    // every shared/dedup'd read in watch.rs too: real upstream backs its
    // shared informer factory with exactly this identity, and its own
    // built-in bootstrap role is deliberately broad on reads for that
    // reason -- see watch.rs's set_base_client doc comment for the full
    // story (docs/E2E_FINDINGS.md finding 22's follow-up).
    let base_client = kube::client::ClientBuilder::try_from(kube_config.clone())
        .context("building apiserver client")?
        .build();
    watch::set_base_client(base_client.clone());
    let election_client = base_client;

    // One impersonated client per controller -- see impersonated_client's
    // doc comment. Built once, up front, so a startup-time HeaderValue
    // error surfaces immediately rather than deep inside a specific
    // controller's own error handling.
    macro_rules! client_for {
        ($name:literal) => {
            impersonated_client(&kube_config, $name)?
        };
    }
    let node_ipam_client = client_for!("node-ipam");
    let node_lifecycle_client = client_for!("node-lifecycle");
    let service_account_client = client_for!("service-account");
    let namespace_client = client_for!("namespace");
    let endpoint_slice_client = client_for!("endpoint-slice");
    let resource_quota_client = client_for!("resource-quota");
    let replica_set_client = client_for!("replica-set");
    let deployment_client = client_for!("deployment");
    let daemon_set_client = client_for!("daemon-set");
    let stateful_set_client = client_for!("stateful-set");
    let garbage_collector_client = client_for!("garbage-collector");
    let job_client = client_for!("job");
    let cron_job_client = client_for!("cron-job");
    let ttl_after_finished_client = client_for!("ttl-after-finished");
    let attach_detach_client = client_for!("attach-detach");
    let pv_binder_client = client_for!("pv-binder");
    let storage_protection_client = client_for!("storage-protection");
    let root_ca_publisher_client = client_for!("root-ca-publisher");
    let resource_claim_client = client_for!("resource-claim");
    let csr_client = client_for!("csr");
    let disruption_client = client_for!("disruption");

    let election_cfg = cfg.election();
    node_leaderelection::run_as_leader(election_client, &election_cfg, || async move {
        tracing::info!("nodecontroller is now leading — starting all controllers");
        tokio::try_join!(
            start_controller(&cfg, "node-ipam", controllers::node_ipam::run(node_ipam_client, &cfg)),
            start_controller(&cfg, "node-lifecycle", controllers::node_lifecycle::run(node_lifecycle_client, &cfg)),
            start_controller(&cfg, "service-account", controllers::service_account::run(service_account_client, &cfg)),
            start_controller(&cfg, "namespace", controllers::namespace::run(namespace_client, &cfg)),
            start_controller(&cfg, "endpoint-slice", controllers::endpoint_slice::run(endpoint_slice_client, &cfg)),
            start_controller(&cfg, "resource-quota", controllers::resource_quota::run(resource_quota_client, &cfg)),
            start_controller(&cfg, "replica-set", controllers::replica_set::run(replica_set_client, &cfg)),
            start_controller(&cfg, "deployment", controllers::deployment::run(deployment_client, &cfg)),
            start_controller(&cfg, "daemon-set", controllers::daemon_set::run(daemon_set_client, &cfg)),
            start_controller(&cfg, "stateful-set", controllers::stateful_set::run(stateful_set_client, &cfg)),
            start_controller(&cfg, "garbage-collector", controllers::garbage_collector::run(garbage_collector_client, &cfg)),
            start_controller(&cfg, "job", controllers::job::run(job_client, &cfg)),
            start_controller(&cfg, "cron-job", controllers::cron_job::run(cron_job_client, &cfg)),
            start_controller(&cfg, "ttl-after-finished", controllers::ttl_after_finished::run(ttl_after_finished_client, &cfg)),
            start_controller(&cfg, "attach-detach", controllers::attach_detach::run(attach_detach_client, &cfg)),
            start_controller(&cfg, "pv-binder", controllers::pv_binder::run(pv_binder_client, &cfg)),
            start_controller(&cfg, "storage-protection", controllers::storage_protection::run(storage_protection_client, &cfg)),
            start_controller(&cfg, "root-ca-publisher", controllers::root_ca_publisher::run(root_ca_publisher_client, &cfg)),
            start_controller(&cfg, "resource-claim", controllers::resource_claim::run(resource_claim_client, &cfg)),
            start_controller(&cfg, "csr", controllers::csr::run(csr_client, &cfg)),
            start_controller(&cfg, "disruption", controllers::disruption::run(disruption_client, &cfg)),
        )?;
        Ok(())
    })
    .await
}
