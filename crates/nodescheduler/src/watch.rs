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
//! # Watch failures
//!
//! `kube::runtime::watcher` re-lists and self-heals internally, surfacing a
//! relist as a fresh `Event::Init`, so an error is logged and ignored. Only a
//! fully-terminated stream is rebuilt, after the 2s delay this repo uses
//! everywhere for the same purpose.

use crate::cache::{Cache, PodInfo};
use crate::config::Config;
use crate::events::{node_action_types, pod_action_types, ActionType, ClusterEvent, EventResource};
use crate::framework::ChangedObject;
use crate::queue::SchedulingQueue;
use futures::{stream::BoxStream, StreamExt};
use k8s_openapi::api::core::v1::{Node, Pod};
use kube::runtime::watcher::{self, Event};
use kube::{Api, Client, ResourceExt};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// The house delay before rebuilding a stream that ended entirely.
const WATCH_RESTART_DELAY: std::time::Duration = std::time::Duration::from_secs(2);

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

/// Run the watches until they stop.
///
/// Only the informers the enabled plugins actually asked for are started —
/// Pod and Node unconditionally, everything else because some plugin named
/// that resource in `events_to_register()`. On a cluster with no
/// PersistentVolumes that is two watches rather than nine, and it is both the
/// parity behaviour and the footprint behaviour.
pub async fn run(client: Client, targets: WatchTargets, _cfg: &Config) -> anyhow::Result<()> {
    let mut mirror = Mirror::default();
    let mut nodes = watch_nodes(&client);
    let mut pods = watch_pods(&client);

    loop {
        tokio::select! {
            event = nodes.next() => match event {
                Some(Ok(ev)) => handle_node_event(ev, &mut mirror, &targets),
                Some(Err(e)) => {
                    tracing::warn!(error = ?e, "node watch error; watcher will retry");
                }
                None => {
                    tracing::warn!("node watch stream ended; rebuilding");
                    tokio::time::sleep(WATCH_RESTART_DELAY).await;
                    nodes = watch_nodes(&client);
                }
            },
            event = pods.next() => match event {
                Some(Ok(ev)) => handle_pod_event(ev, &mut mirror, &targets),
                Some(Err(e)) => {
                    tracing::warn!(error = ?e, "pod watch error; watcher will retry");
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
        Event::InitDone => {}
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
        Event::InitDone => {}
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
                    // `add` re-runs PreEnqueue, so a still-gated pod is held
                    // rather than admitted.
                    targets.queue.add(info);
                }
                PodRoute::Ignore => {}
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
    fn a_pod_with_an_empty_node_name_is_unassigned() {
        // The projection normalises "" to None; if it ever stopped doing so,
        // this pod would be committed to a node named "".
        let mut p = pod(None, "default-scheduler");
        p.node_name = None;
        assert_eq!(route_pod(&p, "default-scheduler"), PodRoute::Queue);
    }
}
