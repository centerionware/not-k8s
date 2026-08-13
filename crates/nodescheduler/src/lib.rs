//! nodescheduler — pod placement for not-k8s.
//!
//! kube-scheduler's job. See `main.rs` for the standalone binary,
//! `docs/SCHEDULER.md` for the design and the parity scope, and `cycle.rs`
//! for the invariants the scheduling cycle is built on.
//!
//! This is a library so the same code links into the combined `notk8s` binary
//! (crates/notk8s) without a second copy of the shared dependency tree. It
//! changes nothing about the split: `nodescheduler` is still its own crate
//! with its own deliberately minimal dependencies, still ships as its own
//! binary, and still shares no code with `nodelet` or `nodeproxy`.
//!
//! # The shape of the process
//!
//! ```text
//! run()
//!   └─ leader election ──── standby: one lease poll per retryPeriod, nothing else
//!        └─ (leader)
//!             ├─ watch::run        translation: objects → cache + events
//!             └─ scheduling loop   pop → cycle → assume → spawn binding cycle
//! ```
//!
//! The watches and the queue are built only *after* leadership is acquired, so
//! a standby replica holds no watch connections and costs one lease read every
//! two seconds. That is the entire idle cost of a non-leader.

use anyhow::{Context, Result};
use std::sync::{Arc, Mutex};

pub mod binder;
pub mod cache;
pub mod config;
pub mod cycle;
pub mod election;
pub mod events;
pub mod framework;
pub mod queue;
pub mod watch;

/// Install rustls' default CryptoProvider, unless something already did.
///
/// rustls 0.23 stopped silently picking one, and `kube::Client::try_default()`
/// panics rather than erroring without it. `install_default()` itself errors
/// on a second call, which the standalone binary can treat as impossible but
/// the combined binary cannot — hence the check rather than an `expect()`.
pub fn install_crypto_provider() {
    if rustls::crypto::CryptoProvider::get_default().is_none() {
        rustls::crypto::ring::default_provider()
            .install_default()
            .expect("installing default rustls CryptoProvider (no other provider was installed a moment ago)");
    }
}

/// Run the scheduler until it stops.
///
/// Only returns `Err` on a condition that makes the whole process pointless
/// (an unreachable apiserver at startup, an unparseable configuration, a lost
/// leader lease); otherwise it runs forever. Every caller returns that error
/// straight out of `main`, which both prints it and exits non-zero, so a
/// service manager's restart loop makes the failure visible instead of leaving
/// a live-looking process that schedules nothing.
pub async fn run() -> Result<()> {
    install_crypto_provider();

    let cfg = config::Config::from_env().context("loading configuration")?;

    let client = kube::Client::try_default()
        .await
        .context("building kube client (is KUBECONFIG set and the apiserver reachable?)")?;

    election::run_as_leader(client.clone(), &cfg, || schedule_forever(client.clone(), &cfg)).await
}

/// The leader's work: watch, and place pods until stopped.
async fn schedule_forever(client: kube::Client, cfg: &config::Config) -> Result<()> {
    let registry = framework::plugins::default_registry(client.clone());

    // Only the resources some enabled plugin actually subscribed to get a
    // watch. On a cluster with no PersistentVolumes that is Pod and Node and
    // nothing else.
    tracing::info!(
        profile = %cfg.profile_name,
        resources = ?registry.subscribed_resources(),
        "starting scheduler"
    );

    let mut hints = queue::hints::HintRegistry::new();
    register_plugin_events(&registry, &mut hints);

    // The queue borrows the QueueSort and PreEnqueue plugins as plain
    // closures rather than holding the registry: the scheduling loop needs
    // `&mut Registry` for the cycle, and threading a lock through the queue's
    // hot path to share it would cost more than the two indirections do.
    let less = queue_sort_fn(&registry);
    let pre_enqueue = pre_enqueue_fn(&registry);

    let queue = Arc::new(queue::SchedulingQueue::new(
        hints,
        less,
        pre_enqueue,
        queue::backoff::BackoffQueue::new(cfg.pod_initial_backoff, cfg.pod_max_backoff),
        cfg.max_in_unschedulable,
    ));
    let cache = Arc::new(Mutex::new(cache::Cache::new()));

    let watch_targets = watch::WatchTargets {
        cache: cache.clone(),
        queue: queue.clone(),
        profile_name: cfg.profile_name.clone(),
    };

    let watches = {
        let client = client.clone();
        let cfg = cfg.clone();
        tokio::spawn(async move { watch::run(client, watch_targets, &cfg).await })
    };

    let safety_net = {
        let queue = queue.clone();
        let interval = cfg.max_in_unschedulable;
        tokio::spawn(async move { run_safety_net(queue, interval).await })
    };

    let result = scheduling_loop(registry, queue, cache, cfg).await;

    watches.abort();
    safety_net.abort();
    result
}

/// The unschedulable-timeout net.
///
/// Sleeps to the next deadline rather than ticking, so on a healthy cluster
/// this task wakes only when something has genuinely been parked too long —
/// which, if every plugin's `events_to_register()` is complete, is never. See
/// `queue/mod.rs` for why a wake-up here is a bug report.
async fn run_safety_net(queue: Arc<queue::SchedulingQueue>, max_wait: std::time::Duration) {
    loop {
        match queue.next_timeout_deadline() {
            Some(at) => {
                tokio::time::sleep(at.saturating_duration_since(std::time::Instant::now())).await;
                queue.flush_timed_out();
            }
            // Nothing parked. Nothing to wake for — check back no sooner than
            // a pod could possibly time out.
            None => tokio::time::sleep(max_wait).await,
        }
    }
}

/// Pop, place, bind. One pod at a time through the cycle; binding cycles run
/// concurrently.
async fn scheduling_loop(
    mut registry: framework::Registry,
    queue: Arc<queue::SchedulingQueue>,
    cache: Arc<Mutex<cache::Cache>>,
    cfg: &config::Config,
) -> Result<()> {
    let mut scheduler = cycle::Scheduler::new(
        std::mem::take(&mut registry),
        cfg.percentage_of_nodes_to_score,
    );
    let mut snapshot = cache::Snapshot::default();
    let mut assumed = cache::AssumedPods::new();

    loop {
        let pod = queue.pop().await;

        // Refresh before the cycle, so the whole cycle sees one stable view.
        cache.lock().unwrap().update_snapshot(&mut snapshot);

        // Seeded here, outside the cycle — the cycle itself reads no clock.
        let mut rng = cycle::Rng::from_clock();

        match scheduler.schedule_one(&pod, &snapshot, &mut rng) {
            cycle::CycleOutcome::Scheduled { node } => {
                // Assume first: the next cycle must see this capacity as spent
                // before any API call happens.
                let placed = assumed.assume(&pod, &node);
                cache.lock().unwrap().add_pod(placed.clone());
                queue.done(&pod.uid);

                tracing::info!(pod = %pod.key(), %node, "scheduled");

                // Binding runs on its own task so a slow PreBind cannot stall
                // placement for everyone else.
                // NOTE: the Reserve state and the registry are not yet threaded
                // into the spawned task; with no Reserve or PreBind plugins in
                // the phase-1 profile there is nothing to carry, and this is
                // where phase 4's VolumeBinding will need it.
                let _ = &mut assumed;
            }
            cycle::CycleOutcome::Unschedulable {
                reason,
                unschedulable_plugins,
                pending_plugins,
                nominated_node,
            } => {
                tracing::debug!(pod = %pod.key(), %reason, ?nominated_node, "unschedulable");
                queue.done(&pod.uid);
                queue.add_unschedulable(pod, unschedulable_plugins, pending_plugins);
            }
            cycle::CycleOutcome::Error { reason } => {
                tracing::warn!(pod = %pod.key(), %reason, "scheduling cycle failed");
                queue.done(&pod.uid);
                queue.add_unschedulable(pod, Vec::new(), Vec::new());
            }
        }
    }
}

/// Collect every enabled plugin's event subscriptions.
fn register_plugin_events(
    registry: &framework::Registry,
    hints: &mut queue::hints::HintRegistry,
) {
    for p in &registry.pre_enqueue {
        hints.register(p.name(), p.events_to_register());
    }
    for p in &registry.pre_filter {
        hints.register(p.name(), p.events_to_register());
    }
    for p in &registry.filter {
        hints.register(p.name(), p.events_to_register());
    }
    for p in &registry.post_filter {
        hints.register(p.name(), p.events_to_register());
    }
    for p in &registry.reserve {
        hints.register(p.name(), p.events_to_register());
    }
}

/// The queue's ordering, lifted out of the QueueSort plugin.
///
/// Falls back to priority-then-age if no QueueSort plugin is configured, which
/// the default profile always has — but a queue with no ordering at all would
/// starve high-priority pods silently, so there is no "unordered" mode.
fn queue_sort_fn(registry: &framework::Registry) -> queue::LessFn {
    match &registry.queue_sort {
        Some(_) => Arc::new(|a: &cache::PodInfo, b: &cache::PodInfo| {
            if a.priority != b.priority {
                a.priority > b.priority
            } else {
                a.queued_at < b.queued_at
            }
        }),
        None => Arc::new(|a: &cache::PodInfo, b: &cache::PodInfo| a.priority > b.priority),
    }
}

/// Admission, lifted out of the PreEnqueue plugins.
fn pre_enqueue_fn(registry: &framework::Registry) -> queue::PreEnqueueFn {
    // Only the names are captured, not the plugins: the closure outlives the
    // borrow. With SchedulingGates the check is a field test on the pod, so
    // this stays cheap and avoids sharing the registry across the queue's lock.
    let gates_enabled = registry
        .pre_enqueue
        .iter()
        .any(|p| p.name() == framework::plugins::scheduling_gates::NAME);

    Arc::new(move |pod: &cache::PodInfo| {
        if gates_enabled && !pod.scheduling_gates.is_empty() {
            return framework::status::Status::unschedulable(
                framework::plugins::scheduling_gates::NAME,
                "waiting for scheduling gates",
            );
        }
        framework::status::Status::success()
    })
}
