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
pub mod report;
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
    // Shared, because the binding cycle runs on its own task and needs the
    // same plugin set the scheduling cycle used.
    let registry = Arc::new(framework::plugins::default_registry(client.clone()));

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

    let queue = Arc::new(queue::SchedulingQueue::new(
        hints,
        queue_sort_fn(registry.clone()),
        pre_enqueue_fn(registry.clone()),
        queue::backoff::BackoffQueue::new(cfg.pod_initial_backoff, cfg.pod_max_backoff),
        cfg.max_in_unschedulable,
    ));
    let cache = Arc::new(Mutex::new(cache::Cache::new()));
    let assumed = Arc::new(Mutex::new(cache::AssumedPods::new()));

    let watch_targets = watch::WatchTargets {
        cache: cache.clone(),
        queue: queue.clone(),
        profile_name: cfg.profile_name.clone(),
    };

    let mut watches = {
        let client = client.clone();
        let cfg = cfg.clone();
        tokio::spawn(async move { watch::run(client, watch_targets, &cfg).await })
    };

    let mut safety_net = {
        let queue = queue.clone();
        let interval = cfg.max_in_unschedulable;
        tokio::spawn(async move { run_safety_net(queue, interval).await })
    };

    // All three are supervised, and any one of them ending ends the process.
    //
    // The first version spawned these two and never looked at them again,
    // only awaiting the scheduling loop. That produces the single worst
    // failure this component has: if the watch task dies, no pod ever
    // reaches the queue, `pop()` blocks forever, and the process sits there
    // holding the leader lease and scheduling nothing — indefinitely, with
    // nothing in the log after the last pod it managed to place. A standby
    // replica cannot take over, because the lease is still being renewed. It
    // is strictly worse than crashing, and it is exactly what a live run did.
    //
    // Exiting instead lets the service manager restart us and the lease
    // lapse, which is the recovery path the whole design already assumes.
    let result = tokio::select! {
        r = scheduling_loop(registry, queue, cache, assumed, client.clone(), cfg) => r,
        r = &mut watches => match r {
            Ok(Ok(())) => Err(anyhow::anyhow!("the watch layer stopped on its own")),
            Ok(Err(e)) => Err(e.context("the watch layer failed")),
            // A panic in the watch task. Surfaced rather than swallowed —
            // this is the case that produced a silent zombie.
            Err(e) => Err(anyhow::anyhow!("the watch layer panicked: {e}")),
        },
        r = &mut safety_net => Err(anyhow::anyhow!(
            "the unschedulable-timeout task stopped ({r:?}); pods parked by an incomplete \
             QueueingHint would now wait forever rather than five minutes"
        )),
    };

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
                tokio::time::sleep_until(at).await;
                queue.flush_timed_out();
            }
            // Nothing parked. Nothing to wake for — check back no sooner than
            // a pod could possibly time out.
            None => tokio::time::sleep(max_wait).await,
        }
    }
}

/// Pop, place, bind. One pod at a time through the cycle; binding cycles run
/// concurrently on their own tasks.
async fn scheduling_loop(
    registry: Arc<framework::Registry>,
    queue: Arc<queue::SchedulingQueue>,
    cache: Arc<Mutex<cache::Cache>>,
    assumed: Arc<Mutex<cache::AssumedPods>>,
    client: kube::Client,
    cfg: &config::Config,
) -> Result<()> {
    let mut scheduler =
        cycle::Scheduler::new(registry.clone(), cfg.percentage_of_nodes_to_score);
    let mut snapshot = cache::Snapshot::default();

    loop {
        let pod = queue.pop().await;

        // Refresh before the cycle, so the whole cycle sees one stable view.
        cache.lock().unwrap().update_snapshot(&mut snapshot);

        // Seeded here, outside the cycle — the cycle itself reads no clock.
        let mut rng = cycle::Rng::from_clock();

        let (outcome, mut state) = scheduler.schedule_one(&pod, &snapshot, &mut rng);

        match outcome {
            cycle::CycleOutcome::Scheduled { node } => {
                // Reserve and Permit run HERE, in the scheduling cycle, while
                // no other pod is mid-cycle — see cycle.rs's header. Only
                // what follows is concurrent.
                let reserved = cycle::run_reserve(&registry, &mut state, &pod, &node);
                if !reserved.is_success() {
                    tracing::info!(pod = %pod.key(), %node, status = %reserved, "reserve rejected");
                    queue.done(&pod.uid);
                    let plugins = if reserved.plugin.is_empty() {
                        Vec::new()
                    } else {
                        vec![reserved.plugin]
                    };
                    queue.add_unschedulable(pod, plugins, Vec::new());
                    continue;
                }

                // Assume before binding: the next cycle must see this capacity
                // as spent before any API call happens.
                let placed = assumed.lock().unwrap().assume(&pod, &node);
                cache.lock().unwrap().add_pod(placed);
                queue.done(&pod.uid);

                tracing::info!(pod = %pod.key(), %node, "scheduled");

                // The binding cycle, on its own task. This is what keeps a
                // PreBind that waits up to 600s for a volume from stalling
                // placement for every other pod in the cluster.
                let registry = registry.clone();
                let queue = queue.clone();
                let cache = cache.clone();
                let assumed = assumed.clone();
                tokio::spawn(async move {
                    let outcome =
                        binder::bind_one(&registry, state, pod.clone(), node).await;
                    binder::handle_outcome(&outcome, pod, &queue, &assumed, &cache);
                });
            }
            cycle::CycleOutcome::Unschedulable {
                reason,
                unschedulable_plugins,
                pending_plugins,
                nominated_node,
            } => {
                // Info rather than debug. This is *the* answer to "why is my
                // pod Pending", and burying it below the default filter meant
                // the one thing an operator needs was invisible without a
                // restart at a different log level.
                tracing::info!(pod = %pod.key(), %reason, ?nominated_node, "unschedulable");
                queue.done(&pod.uid);

                // Tell the cluster why, off the scheduling loop. Without this
                // the pod sits Pending with an empty Events section and no
                // conditions, and there is no way to tell "nothing has room"
                // from "the scheduler is not running" — see report.rs.
                let client = client.clone();
                let reported = pod.clone();
                let profile = cfg.profile_name.clone();
                let reason_text = reason.clone();
                let nominated = nominated_node.clone();
                tokio::spawn(async move {
                    report::report_unschedulable(
                        &client,
                        &reported,
                        &reason_text,
                        nominated.as_deref(),
                        &profile,
                    )
                    .await;
                });

                queue.add_unschedulable(pod, unschedulable_plugins, pending_plugins);
            }
            cycle::CycleOutcome::Error { reason } => {
                tracing::warn!(pod = %pod.key(), %reason, "scheduling cycle failed");
                queue.done(&pod.uid);
                // Reported too: a plugin malfunction leaves the pod just as
                // Pending as a genuine rejection does, and the user is owed
                // the same explanation either way.
                let client = client.clone();
                let reported = pod.clone();
                let profile = cfg.profile_name.clone();
                let reason_text = reason.clone();
                tokio::spawn(async move {
                    report::report_unschedulable(&client, &reported, &reason_text, None, &profile)
                        .await;
                });
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

/// The queue's ordering — the QueueSort plugin itself, not a copy of it.
///
/// Holding the registry in the closure rather than reimplementing the
/// comparison is the whole point: a second copy of the ordering rule would
/// drift from the plugin silently, and the symptom (pods served in the wrong
/// order under contention) is one nobody attributes to a duplicated
/// comparator.
fn queue_sort_fn(registry: Arc<framework::Registry>) -> queue::LessFn {
    Arc::new(move |a: &cache::PodInfo, b: &cache::PodInfo| match &registry.queue_sort {
        Some(plugin) => plugin.less(a, b),
        // Structurally impossible with the default profile, which always has
        // PrioritySort — but an unordered queue would starve high-priority
        // pods silently, so there is no "unordered" mode even here.
        None => a.priority > b.priority,
    })
}

/// Admission — every PreEnqueue plugin, in order.
///
/// Same reasoning as `queue_sort_fn`: this runs the real plugins rather than
/// re-testing `scheduling_gates` by hand, so a new PreEnqueue plugin is
/// honoured by the queue the moment it is added to the profile.
fn pre_enqueue_fn(registry: Arc<framework::Registry>) -> queue::PreEnqueueFn {
    Arc::new(move |pod: &cache::PodInfo| {
        for plugin in &registry.pre_enqueue {
            let status = plugin.pre_enqueue(pod);
            if !status.is_success() {
                return status;
            }
        }
        framework::status::Status::success()
    })
}
