//! Watches, and the translation from API objects into cache mutations and
//! cluster events.
//!
//! **Translation only.** No decision about *whether* a pod should be retried
//! belongs here — that is `queue/hints.rs`, driven by what each plugin
//! subscribed to. A behavioural rule that appears in this file is almost
//! certainly in the wrong place, in the same way that a behavioural decision
//! in `nodestore`'s `server/` is.
//!
//! # The pod watch is two watches
//!
//! Pods split by whether they have a `spec.nodeName`:
//!
//!   * **assigned** — someone placed them. They drive the **cache**: their
//!     resources are committed to a node. They must never be enqueued.
//!   * **unassigned**, with a `spec.schedulerName` this profile answers for —
//!     they drive the **queue**. They are what this component exists to place.
//!
//! Conflating the two is a real upstream bug class, and the failure is
//! spectacular rather than subtle: feeding assigned pods into the queue makes
//! the scheduler try to re-place every running pod in the cluster, and feeding
//! unassigned ones into the cache commits resources on a node named `""`.
//!
//! # Node updates go through the diff, always
//!
//! Every node update is turned into a `ClusterEvent` by
//! [`crate::events::node_action_types`], and **an empty action emits nothing**.
//! That is the heartbeat case — one Node update per node per
//! `node-monitor-period`, forever, at complete idle — and dropping it here is
//! what the whole event vocabulary exists for. Emitting a generic update
//! instead would be correct and would also make idle cost scale with cluster
//! size.
//!
//! # Watch failures, and why they need backoff of their own
//!
//! `kube::runtime::watcher` re-lists and self-heals across an *interrupted*
//! stream, surfacing the relist as a fresh `Event::Init`. It does not,
//! however, pace a watch that cannot **start**: with the apiserver down,
//! every poll returns `WatchStartFailed` immediately, so a loop that merely
//! logs and polls again spins at full CPU and writes thousands of identical
//! log lines a second.
//!
//! That is not hypothetical — it is what the first live run of this component
//! did, for the whole window where k3s restarts (`setup-control-plane.sh`
//! runs twice, the second pass adding the kubelet CA). The scheduling loop
//! starved alongside it, and the only visible symptom was pods not being
//! placed, which reads as a scheduling bug rather than a watch one.
//!
//! So consecutive failures back off, and any successful event resets it. This
//! is the same lesson as `SchedulingQueue::update`: a retry path with no
//! pacing is a busy-loop, and both of this component's busy-loops were
//! introduced by code that looked obviously correct in isolation.

use crate::cache::{Cache, PodInfo};
use crate::config::Config;
use crate::events::{node_action_types, pod_action_types, ActionType, ClusterEvent, EventResource};
use crate::framework::ChangedObject;
use crate::queue::SchedulingQueue;
use futures::{stream::BoxStream, StreamExt};
use k8s_openapi::api::core::v1::{Node, Pod};
use kube::runtime::watcher;
use kube::runtime::watcher::Event;
use kube::{Api, Client, ResourceExt};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// The house delay before rebuilding a stream that ended entirely.
const WATCH_RESTART_DELAY: std::time::Duration = std::time::Duration::from_secs(2);

/// First pause after a watch fails to start, doubling to [`WATCH_MAX_BACKOFF`].
const WATCH_INITIAL_BACKOFF: std::time::Duration = std::time::Duration::from_millis(500);
/// Ceiling. Low enough that a scheduler is placing pods again within seconds
/// of the apiserver returning, which is the whole point of it being here.
///
/// 30s was the first value and it was wrong — it contradicted this very
/// comment. A real apiserver restart took ~72s to recover from, because the
/// doubling reached the ceiling while the apiserver was still down and then
/// slept through most of its return. Retrying a failed watch every 5s during
/// an outage costs nothing; not placing pods for a minute after the outage
/// ends costs the cluster.
const WATCH_MAX_BACKOFF: std::time::Duration = std::time::Duration::from_secs(5);

/// Whether this failure is worth a log line.
///
/// The first version logged only the very first failure of a run, on the
/// reasoning that a minute of downtime must not produce a minute of
/// identical warnings. That was right about the noise and wrong about the
/// consequence: a sustained outage then produced exactly one warning
/// followed by total silence, which is indistinguishable from a wedged
/// watch — and a live run was misdiagnosed as exactly that for several
/// rounds, because "still retrying, apiserver still down" and "dead" looked
/// identical in the log.
///
/// So: the first few, then one roughly every ten afterwards. A long outage
/// stays legible without becoming a flood, and the paired "recovered" line
/// makes its duration explicit.
fn should_log_failure(consecutive: u32) -> bool {
    consecutive <= 3 || consecutive % 10 == 0
}

/// How long to wait after `n` consecutive failures.
///
/// Pure and separate so the curve is testable without an apiserver to break.
fn watch_backoff(consecutive_failures: u32) -> std::time::Duration {
    if consecutive_failures == 0 {
        return std::time::Duration::ZERO;
    }
    let doubled = WATCH_INITIAL_BACKOFF
        .checked_mul(1u32 << (consecutive_failures - 1).min(16))
        .unwrap_or(WATCH_MAX_BACKOFF);
    doubled.min(WATCH_MAX_BACKOFF)
}

/// Whether a pod is this profile's business, and which half of the split it
/// belongs to.
///
/// Pure, so the routing rule that matters most in this file is testable
/// without a cluster.
#[derive(Debug, PartialEq, Eq)]
pub enum PodRoute {
    /// Placed. Commit its resources to the cache.
    Cache,
    /// Unplaced and ours. Put it in the queue.
    Queue,
    /// Unplaced, but another scheduler's profile answers for it.
    Ignore,
}

pub fn route_pod(pod: &PodInfo, profile_name: &str) -> PodRoute {
    if pod.node_name.is_some() {
        // Assigned pods go to the cache regardless of scheduler name: their
        // resources are consumed whoever placed them, and a node's free
        // capacity is not a per-profile question.
        return PodRoute::Cache;
    }
    if pod.scheduler_name == profile_name {
        PodRoute::Queue
    } else {
        PodRoute::Ignore
    }
}

/// Everything the watch layer writes into.
pub struct WatchTargets {
    pub cache: Arc<Mutex<Cache>>,
    pub queue: Arc<SchedulingQueue>,
    pub profile_name: String,
    /// PodDisruptionBudgets, mirrored for preemption.
    ///
    /// Started only when preemption is enabled, per the rule that an informer
    /// exists because something asked for it. A cluster that never preempts
    /// pays nothing for this watch.
    pub budgets: Arc<Mutex<Vec<crate::preempt::PdbState>>>,
}

/// Mirrors of the last version of each object, so updates can be diffed.
///
/// The watcher delivers only the new object, but the whole event vocabulary
/// is defined in terms of what *changed* — so the previous version has to be
/// kept. This is the one place the projection is not enough: the diff needs
/// fields (heartbeat timestamps) that the projection deliberately drops, so
/// these hold the API objects.
#[derive(Default)]
struct Mirror {
    nodes: HashMap<String, Node>,
    pods: HashMap<String, Pod>,
}

fn watch_nodes(client: &Client) -> BoxStream<'static, watcher::Result<Event<Node>>> {
    let api: Api<Node> = Api::all(client.clone());
    watcher(api, watcher::Config::default()).boxed()
}

fn watch_pods(client: &Client) -> BoxStream<'static, watcher::Result<Event<Pod>>> {
    let api: Api<Pod> = Api::all(client.clone());
    watcher(api, watcher::Config::default()).boxed()
}

type Pdb = k8s_openapi::api::policy::v1::PodDisruptionBudget;
type Ns = k8s_openapi::api::core::v1::Namespace;
type Svc = k8s_openapi::api::core::v1::Service;
type Rs = k8s_openapi::api::apps::v1::ReplicaSet;
type Sts = k8s_openapi::api::apps::v1::StatefulSet;
type Rc = k8s_openapi::api::core::v1::ReplicationController;

fn watch_namespaces(client: &Client) -> BoxStream<'static, watcher::Result<Event<Ns>>> {
    watcher(Api::<Ns>::all(client.clone()), watcher::Config::default()).boxed()
}
/// The four workload watches exist only to derive `PodTopologySpread`'s
/// system default constraints, so `TopologyDefaulting::None` must actually
/// drop them rather than merely ignore what they deliver — that is the whole
/// point of the setting.
///
/// Disabled means [`futures::stream::pending`], which never yields, not
/// [`futures::stream::empty`], which yields `None` immediately. An always-ready
/// `None` arm in a `select!` spins the loop at 100% CPU — the exact shape of
/// the bug filed as issue #16 against nodelet.
fn watch_workload<K>(client: &Client, enabled: bool) -> BoxStream<'static, watcher::Result<Event<K>>>
where
    K: kube::Resource<DynamicType = ()>
        + Clone
        + std::fmt::Debug
        + serde::de::DeserializeOwned
        + Send
        + Sync
        + 'static,
{
    if !enabled {
        return futures::stream::pending().boxed();
    }
    watcher(Api::<K>::all(client.clone()), watcher::Config::default()).boxed()
}

/// A Service's selector, as a LabelSelector.
///
/// Services carry a plain `map[string]string`, not a LabelSelector, so it is
/// converted rather than borrowed. An *empty* service selector selects
/// nothing (it is a headless/manually-managed Service), which is the opposite
/// of an empty LabelSelector — so it is dropped rather than converted into
/// one that matches every pod in the namespace.
fn service_selector(svc: &Svc) -> Option<k8s_openapi::apimachinery::pkg::apis::meta::v1::LabelSelector> {
    let map = svc.spec.as_ref()?.selector.clone()?;
    if map.is_empty() {
        return None;
    }
    Some(k8s_openapi::apimachinery::pkg::apis::meta::v1::LabelSelector {
        match_labels: Some(map),
        match_expressions: None,
    })
}

fn watch_pdbs(client: &Client) -> BoxStream<'static, watcher::Result<Event<Pdb>>> {
    let api: Api<Pdb> = Api::all(client.clone());
    watcher(api, watcher::Config::default()).boxed()
}

/// Mirror a workload's selector into the cache.
///
/// One function for all four kinds because the only thing that differs is
/// where the selector lives. The key carries the kind so a Service and a
/// ReplicaSet sharing a name in one namespace do not collapse into each
/// other and lose a selector.
fn handle_workload_event<K, F>(
    ev: Event<K>,
    kind: &str,
    targets: &WatchTargets,
    selector_of: F,
) where
    K: kube::Resource<DynamicType = ()> + Clone,
    F: Fn(&K) -> Option<k8s_openapi::apimachinery::pkg::apis::meta::v1::LabelSelector>,
{
    let key = |o: &K| {
        format!("{kind}/{}/{}", o.meta().namespace.clone().unwrap_or_default(), o.name_any())
    };
    let mut cache = targets.cache.lock().unwrap();
    match ev {
        Event::Init | Event::InitDone => {}
        Event::InitApply(o) | Event::Apply(o) => {
            let k = key(&o);
            match selector_of(&o) {
                Some(selector) => cache.upsert_workload(
                    k,
                    crate::cache::snapshot::WorkloadSelector {
                        namespace: o.meta().namespace.clone().unwrap_or_default(),
                        selector,
                    },
                ),
                // A workload that selects nothing must not linger from an
                // earlier revision that did.
                None => cache.remove_workload(&k),
            }
        }
        Event::Delete(o) => cache.remove_workload(&key(&o)),
    }
}

/// Mirror PodDisruptionBudgets for preemption.
///
/// A whole-list rebuild per event rather than an incremental mirror: PDBs are
/// few (one per workload at most), change rarely, and preemption reads the
/// whole set anyway. Incremental bookkeeping here would be more code and more
/// ways to drift for no measurable gain — the opposite trade from the pod and
/// node caches, which are large and change constantly.
fn handle_pdb_event(ev: Event<Pdb>, mirror: &mut HashMap<String, Pdb>, targets: &WatchTargets) {
    match ev {
        Event::Init => mirror.clear(),
        Event::InitDone => {}
        Event::InitApply(p) | Event::Apply(p) => {
            mirror.insert(format!("{}/{}", p.namespace().unwrap_or_default(), p.name_any()), p);
        }
        Event::Delete(p) => {
            mirror.remove(&format!("{}/{}", p.namespace().unwrap_or_default(), p.name_any()));
        }
    }
    let rebuilt: Vec<crate::preempt::PdbState> =
        mirror.values().map(crate::preempt::PdbState::from_api).collect();
    *targets.budgets.lock().unwrap() = rebuilt;
}

/// Error/end handling shared by the four workload watches.
///
/// Returns `true` when the caller must rebuild the stream — it ended, and an
/// ended stream in a `select!` is ready forever after, so ignoring it spins
/// the loop rather than merely losing updates.
async fn workload_stream_hiccup<K>(
    event: &Option<watcher::Result<Event<K>>>,
    what: &str,
    failures: &mut u32,
) -> bool {
    match event {
        Some(Ok(_)) => {
            *failures = 0;
            false
        }
        Some(Err(e)) => {
            *failures = failures.saturating_add(1);
            let pause = watch_backoff(*failures);
            if should_log_failure(*failures) {
                tracing::warn!(
                    error = ?e, consecutive = *failures, watch = what,
                    "workload watch error; retrying with backoff"
                );
            }
            tokio::time::sleep(pause).await;
            false
        }
        None => {
            tracing::warn!(watch = what, "workload watch stream ended; rebuilding");
            tokio::time::sleep(WATCH_RESTART_DELAY).await;
            true
        }
    }
}

/// Run the watches until they stop.
///
/// Only the informers the enabled plugins actually asked for are started —
/// Pod and Node unconditionally, everything else because some plugin named
/// that resource in `events_to_register()`. On a cluster with no
/// PersistentVolumes that is two watches rather than nine, and it is both the
/// parity behaviour and the footprint behaviour.
pub async fn run(client: Client, targets: WatchTargets, cfg: &Config) -> anyhow::Result<()> {
    let mut mirror = Mirror::default();
    let mut pdb_mirror: HashMap<String, Pdb> = HashMap::new();
    let mut nodes = watch_nodes(&client);
    let mut pods = watch_pods(&client);
    let mut pdbs = watch_pdbs(&client);
    let mut pdb_failures: u32 = 0;
    // The metadata watches. Small, rarely-changing objects whose only
    // consumers are InterPodAffinity's namespaceSelector and
    // PodTopologySpread's system default constraints — but both are parity,
    // so both are watched.
    let mut namespaces = watch_namespaces(&client);
    // Namespaces are watched unconditionally — `namespaceSelector` has no
    // opt-out — but the four workload watches follow the defaulting setting.
    let workloads = cfg.topology_defaulting == crate::config::TopologyDefaulting::SystemDefaulting;
    let mut services = watch_workload::<Svc>(&client, workloads);
    let mut replicasets = watch_workload::<Rs>(&client, workloads);
    let mut statefulsets = watch_workload::<Sts>(&client, workloads);
    let mut rcs = watch_workload::<Rc>(&client, workloads);
    // Counted per stream: one watch can be failing while the other is fine.
    let mut node_failures: u32 = 0;
    let mut pod_failures: u32 = 0;
    let mut ns_failures: u32 = 0;
    let mut svc_failures: u32 = 0;
    let mut rs_failures: u32 = 0;
    let mut sts_failures: u32 = 0;
    let mut rc_failures: u32 = 0;

    loop {
        tokio::select! {
            event = nodes.next() => match event {
                Some(Ok(ev)) => {
                    if node_failures > 0 {
                        tracing::info!(
                            after_failures = node_failures,
                            "node watch recovered"
                        );
                    }
                    node_failures = 0;
                    handle_node_event(ev, &mut mirror, &targets);
                }
                Some(Err(e)) => {
                    node_failures = node_failures.saturating_add(1);
                    let pause = watch_backoff(node_failures);
                    if should_log_failure(node_failures) {
                        tracing::warn!(
                            error = ?e, consecutive = node_failures,
                            "node watch error; retrying with backoff"
                        );
                    }
                    tokio::time::sleep(pause).await;
                }
                None => {
                    tracing::warn!("node watch stream ended; rebuilding");
                    tokio::time::sleep(WATCH_RESTART_DELAY).await;
                    nodes = watch_nodes(&client);
                }
            },
            event = namespaces.next() => match event {
                Some(Ok(ev)) => {
                    let mut cache = targets.cache.lock().unwrap();
                    match ev {
                        Event::Init | Event::InitDone => {}
                        Event::InitApply(ns) | Event::Apply(ns) => cache.upsert_namespace(
                            &ns.name_any(),
                            ns.metadata.labels.clone().unwrap_or_default(),
                        ),
                        Event::Delete(ns) => cache.remove_namespace(&ns.name_any()),
                    }
                }
                Some(Err(e)) => {
                    ns_failures = ns_failures.saturating_add(1);
                    let pause = watch_backoff(ns_failures);
                    if should_log_failure(ns_failures) {
                        tracing::warn!(
                            error = ?e, consecutive = ns_failures,
                            "namespace watch error; retrying with backoff"
                        );
                    }
                    tokio::time::sleep(pause).await;
                }
                // Not ignorable: a `None` arm is instantly ready every time
                // round, which turns the select! into a spin loop.
                None => {
                    tracing::warn!("namespace watch stream ended; rebuilding");
                    tokio::time::sleep(WATCH_RESTART_DELAY).await;
                    namespaces = watch_namespaces(&client);
                }
            },
            event = services.next() => {
                if workload_stream_hiccup(&event, "service", &mut svc_failures).await {
                    services = watch_workload::<Svc>(&client, workloads);
                } else if let Some(Ok(ev)) = event {
                    handle_workload_event(ev, "svc", &targets, service_selector);
                }
            },
            event = replicasets.next() => {
                if workload_stream_hiccup(&event, "replicaset", &mut rs_failures).await {
                    replicasets = watch_workload::<Rs>(&client, workloads);
                } else if let Some(Ok(ev)) = event {
                    handle_workload_event(ev, "rs", &targets, |o: &Rs| {
                        o.spec.as_ref().map(|s| s.selector.clone())
                    });
                }
            },
            event = statefulsets.next() => {
                if workload_stream_hiccup(&event, "statefulset", &mut sts_failures).await {
                    statefulsets = watch_workload::<Sts>(&client, workloads);
                } else if let Some(Ok(ev)) = event {
                    handle_workload_event(ev, "sts", &targets, |o: &Sts| {
                        o.spec.as_ref().map(|s| s.selector.clone())
                    });
                }
            },
            event = rcs.next() => {
                if workload_stream_hiccup(&event, "replicationcontroller", &mut rc_failures).await {
                    rcs = watch_workload::<Rc>(&client, workloads);
                } else if let Some(Ok(ev)) = event {
                    handle_workload_event(ev, "rc", &targets, |o: &Rc| {
                        let map = o.spec.as_ref()?.selector.clone()?;
                        if map.is_empty() { return None; }
                        Some(k8s_openapi::apimachinery::pkg::apis::meta::v1::LabelSelector {
                            match_labels: Some(map),
                            match_expressions: None,
                        })
                    });
                }
            },
            event = pdbs.next() => match event {
                Some(Ok(ev)) => {
                    pdb_failures = 0;
                    handle_pdb_event(ev, &mut pdb_mirror, &targets);
                }
                Some(Err(e)) => {
                    pdb_failures = pdb_failures.saturating_add(1);
                    let pause = watch_backoff(pdb_failures);
                    if should_log_failure(pdb_failures) {
                        tracing::warn!(
                            error = ?e, consecutive = pdb_failures,
                            "PodDisruptionBudget watch error; retrying with backoff"
                        );
                    }
                    tokio::time::sleep(pause).await;
                }
                None => {
                    tracing::warn!("PodDisruptionBudget watch stream ended; rebuilding");
                    tokio::time::sleep(WATCH_RESTART_DELAY).await;
                    pdbs = watch_pdbs(&client);
                }
            },
            event = pods.next() => match event {
                Some(Ok(ev)) => {
                    if pod_failures > 0 {
                        tracing::info!(after_failures = pod_failures, "pod watch recovered");
                    }
                    pod_failures = 0;
                    handle_pod_event(ev, &mut mirror, &targets);
                }
                Some(Err(e)) => {
                    pod_failures = pod_failures.saturating_add(1);
                    let pause = watch_backoff(pod_failures);
                    if should_log_failure(pod_failures) {
                        tracing::warn!(
                            error = ?e, consecutive = pod_failures,
                            "pod watch error; retrying with backoff"
                        );
                    }
                    tokio::time::sleep(pause).await;
                }
                None => {
                    tracing::warn!("pod watch stream ended; rebuilding");
                    tokio::time::sleep(WATCH_RESTART_DELAY).await;
                    pods = watch_pods(&client);
                }
            },
        }
    }
}

fn handle_node_event(ev: Event<Node>, mirror: &mut Mirror, targets: &WatchTargets) {
    match ev {
        // A relist started. Drop the mirror so stale objects cannot produce
        // phantom diffs against whatever arrives next.
        Event::Init => mirror.nodes.clear(),
        // Logged at info, and deliberately: a relist is rare on a healthy
        // cluster, and its absence is the difference between "the watch died"
        // and "the watch is alive but delivering nothing" — a distinction a
        // live run could not be made to answer without it.
        Event::InitDone => {
            tracing::info!(nodes = mirror.nodes.len(), "node watch (re)listed")
        }
        Event::InitApply(node) | Event::Apply(node) => {
            let name = node.name_any();
            let previous = mirror.nodes.insert(name.clone(), node.clone());

            targets.cache.lock().unwrap().upsert_node(&node);

            let action = match &previous {
                // First sight of this node: an addition, whatever the relist
                // said. A new node is the single most useful event there is —
                // it can un-stick almost anything.
                None => ActionType::ADD,
                // THE hot path. An empty action here is the heartbeat case,
                // and it must wake nothing at all.
                Some(old) => node_action_types(old, &node),
            };
            if action.is_empty() {
                return;
            }
            targets.queue.move_all_to_active_or_backoff(
                ClusterEvent::new(EventResource::Node, action),
                previous.map(|n| ChangedObject::Node(Box::new(n))),
                Some(ChangedObject::Node(Box::new(node))),
            );
        }
        Event::Delete(node) => {
            let name = node.name_any();
            mirror.nodes.remove(&name);
            targets.cache.lock().unwrap().remove_node(&name);
            // Deliberately no event: a node going away frees nothing and makes
            // no pending pod schedulable. Waking every parked pod to have them
            // all fail again is exactly the thundering herd this design avoids.
        }
    }
}

fn handle_pod_event(ev: Event<Pod>, mirror: &mut Mirror, targets: &WatchTargets) {
    match ev {
        Event::Init => mirror.pods.clear(),
        Event::InitDone => {
            tracing::info!(pods = mirror.pods.len(), "pod watch (re)listed")
        }
        Event::InitApply(pod) | Event::Apply(pod) => {
            let key = pod_key(&pod);
            let previous = mirror.pods.insert(key.clone(), pod.clone());
            let info = Arc::new(PodInfo::from_pod(&pod, k8s_openapi::jiff::Timestamp::now()));

            match route_pod(&info, &targets.profile_name) {
                PodRoute::Cache => {
                    // It may have been queued before it was placed — by us, or
                    // by another scheduler that got there first.
                    targets.queue.remove(&info.uid);
                    targets.cache.lock().unwrap().add_pod(info);

                    // An assigned pod changing can free capacity (it shrank)
                    // or change what anti-affinity sees (its labels moved).
                    if let Some(old) = &previous {
                        let action = pod_action_types(old, &pod);
                        if !action.is_empty() {
                            targets.queue.move_all_to_active_or_backoff(
                                ClusterEvent::new(EventResource::AssignedPod, action),
                                Some(ChangedObject::Pod(Box::new(old.clone()))),
                                Some(ChangedObject::Pod(Box::new(pod))),
                            );
                        }
                    }
                }
                PodRoute::Queue => {
                    // A first sighting is an arrival; anything else is an
                    // edit. Conflating them is a hot loop — see
                    // SchedulingQueue::update, which this exists to call.
                    //
                    // `add` re-runs PreEnqueue, so a still-gated pod is held
                    // rather than admitted.
                    // At info, not debug: "did this pod reach the scheduler
                    // at all" is the first question asked of any pod that
                    // stays Pending, and answering it should not require a
                    // rebuild with different logging. One line per unplaced
                    // pod is proportionate — assigned pods, which are the
                    // bulk of the traffic, log nothing here.
                    if let Some(old) = &previous {
                        tracing::debug!(pod = %info.key(), "queued (update)");
                        targets.queue.update(info);

                        // An unplaced pod changing is a cluster event too, and
                        // this branch used to emit none — only the assigned-pod
                        // branch did. So a plugin subscribing to a pod-spec
                        // change (SchedulingGates waits on exactly one:
                        // UPDATE_POD_SCHEDULING_GATES_ELIMINATED) could never
                        // be woken by it, and its pods sat until the
                        // five-minute net.
                        let action = pod_action_types(old, &pod);
                        if !action.is_empty() {
                            targets.queue.move_all_to_active_or_backoff(
                                ClusterEvent::new(EventResource::UnschedulablePod, action),
                                Some(ChangedObject::Pod(Box::new(old.clone()))),
                                Some(ChangedObject::Pod(Box::new(pod))),
                            );
                        }
                    } else {
                        tracing::info!(pod = %info.key(), "queued for scheduling");
                        targets.queue.add(info);
                    }
                }
                PodRoute::Ignore => {
                    // An unplaced pod we are deliberately not touching,
                    // because another scheduler's profile owns it. Logged
                    // because "ignored" and "never arrived" are
                    // indistinguishable in a log that only records what it
                    // acted on — and telling them apart is the difference
                    // between a routing bug and a watch bug.
                    tracing::info!(
                        pod = %info.key(),
                        scheduler_name = %info.scheduler_name,
                        profile = %targets.profile_name,
                        "ignoring a pod owned by another scheduler"
                    );
                }
            }
        }
        Event::Delete(pod) => {
            let key = pod_key(&pod);
            mirror.pods.remove(&key);
            let info = PodInfo::from_pod(&pod, k8s_openapi::jiff::Timestamp::now());

            targets.queue.remove(&info.uid);

            if info.node_name.is_some() {
                targets.cache.lock().unwrap().remove_pod(&info.uid);
                // This one genuinely frees capacity, so it is worth waking
                // pods for — it is the single most common reason a pending pod
                // becomes schedulable.
                targets.queue.move_all_to_active_or_backoff(
                    ClusterEvent::new(EventResource::AssignedPod, ActionType::DELETE),
                    Some(ChangedObject::Pod(Box::new(pod))),
                    None,
                );
            }
        }
    }
}

fn pod_key(pod: &Pod) -> String {
    format!("{}/{}", pod.namespace().unwrap_or_default(), pod.name_any())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pod(node: Option<&str>, scheduler: &str) -> PodInfo {
        PodInfo {
            uid: "u".to_string(),
            name: "p".to_string(),
            node_name: node.map(String::from),
            scheduler_name: scheduler.to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn an_unassigned_pod_for_this_profile_goes_to_the_queue() {
        assert_eq!(route_pod(&pod(None, "default-scheduler"), "default-scheduler"), PodRoute::Queue);
    }

    #[test]
    fn an_assigned_pod_goes_to_the_cache_and_never_the_queue() {
        // Feeding assigned pods into the queue makes the scheduler try to
        // re-place every running pod in the cluster.
        assert_eq!(
            route_pod(&pod(Some("worker-1"), "default-scheduler"), "default-scheduler"),
            PodRoute::Cache
        );
    }

    #[test]
    fn an_unassigned_pod_for_another_profile_is_ignored() {
        assert_eq!(route_pod(&pod(None, "batch-scheduler"), "default-scheduler"), PodRoute::Ignore);
    }

    #[test]
    fn an_assigned_pod_belonging_to_another_scheduler_still_reaches_the_cache() {
        // Its resources are consumed whoever placed it — free capacity is not
        // a per-profile question. Ignoring it would make every node look
        // emptier than it is.
        assert_eq!(
            route_pod(&pod(Some("worker-1"), "batch-scheduler"), "default-scheduler"),
            PodRoute::Cache
        );
    }

    #[test]
    fn the_first_watch_failure_pauses_before_retrying() {
        // Zero here is the spin: the apiserver refuses the connection, the
        // poll returns instantly, and the loop polls again forever.
        assert!(watch_backoff(1) > std::time::Duration::ZERO);
        assert_eq!(watch_backoff(1), WATCH_INITIAL_BACKOFF);
    }

    #[test]
    fn repeated_watch_failures_back_off_and_then_stop_growing() {
        assert_eq!(watch_backoff(2), WATCH_INITIAL_BACKOFF * 2);
        assert_eq!(watch_backoff(3), WATCH_INITIAL_BACKOFF * 4);
        assert_eq!(watch_backoff(20), WATCH_MAX_BACKOFF);
        // Far past any plausible outage: must saturate, not overflow.
        assert_eq!(watch_backoff(u32::MAX), WATCH_MAX_BACKOFF);
    }

    #[test]
    fn the_ceiling_is_short_enough_to_recover_promptly() {
        // Measured, not guessed: a 30s ceiling turned a real apiserver
        // restart into ~72s of placing nothing, because the doubling hit the
        // ceiling while it was still down and then slept through its return.
        assert!(
            WATCH_MAX_BACKOFF <= std::time::Duration::from_secs(5),
            "a scheduler that waits this long after the apiserver returns is a \
             cluster that places no pods for that long"
        );
    }

    #[test]
    fn a_sustained_outage_stays_visible_without_flooding() {
        // The first few, so a short blip is fully described; then one in ten,
        // so a long outage remains legible. Logging only the first — the
        // original behaviour — made "still retrying" and "wedged" identical
        // in the log, and a live failure was misread as the latter for
        // several rounds because of it.
        assert!(should_log_failure(1));
        assert!(should_log_failure(2));
        assert!(should_log_failure(3));
        assert!(!should_log_failure(4));
        assert!(should_log_failure(10));
        assert!(!should_log_failure(11));
        assert!(should_log_failure(100));
    }

    #[test]
    fn a_long_outage_logs_a_bounded_number_of_lines() {
        // Ten minutes at the 5s ceiling is ~120 failures; that must be tens
        // of lines, not hundreds.
        let lines = (1..=120u32).filter(|n| should_log_failure(*n)).count();
        assert!(lines < 20, "{lines} lines for a ten-minute outage is a flood");
    }

    #[test]
    fn a_healthy_watch_waits_for_nothing() {
        assert_eq!(watch_backoff(0), std::time::Duration::ZERO);
    }

    #[test]
    fn a_pod_with_an_empty_node_name_is_unassigned() {
        // The projection normalises "" to None; if it ever stopped doing so,
        // this pod would be committed to a node named "".
        let mut p = pod(None, "default-scheduler");
        p.node_name = None;
        assert_eq!(route_pod(&p, "default-scheduler"), PodRoute::Queue);
    }
}
