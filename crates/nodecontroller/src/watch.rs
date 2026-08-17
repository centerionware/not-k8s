//! The Node watch, and the backoff that paces it when it cannot even start.
//!
//! Copied from `nodescheduler::watch`'s `WatchBackoffPolicy`/`watch_nodes`
//! rather than shared via a dependency: it's ~30 lines, and the two crates
//! having no dependency on each other (only both depending on
//! `node-leaderelection`) is a deliberate property worth the small
//! duplication — see CLAUDE.md's crate-isolation reasoning for
//! `nodeproxy`/`nodelet`, same argument applies laterally here.
//!
//! `kube::runtime::watcher` re-lists and self-heals across an *interrupted*
//! stream, but does not pace a watch that cannot **start** — with the
//! apiserver down, every poll returns `WatchStartFailed` immediately, and a
//! loop that merely logs and polls again spins at full CPU. This is what
//! `nodescheduler` found live in its first real run (see that crate's
//! `watch.rs` for the incident); this component starts from the fix rather
//! than rediscovering the bug.

use futures::stream::BoxStream;
use futures::StreamExt;
use k8s_openapi::api::apps::v1::{DaemonSet, Deployment, ReplicaSet, StatefulSet};
use k8s_openapi::api::batch::v1::{CronJob, Job};
use k8s_openapi::api::certificates::v1::CertificateSigningRequest;
use k8s_openapi::api::coordination::v1::Lease;
use k8s_openapi::api::core::v1::{Namespace, Node, PersistentVolume, PersistentVolumeClaim, Pod, ResourceQuota, Service};
use k8s_openapi::api::policy::v1::PodDisruptionBudget;
use k8s_openapi::api::storage::v1::VolumeAttachment;
use kube::runtime::utils::{Backoff, WatchStreamExt};
use kube::runtime::watcher;
use kube::runtime::watcher::Event;
use kube::{Api, Client, Resource, ResourceExt};
use serde::de::DeserializeOwned;
use std::collections::HashMap;
use std::fmt::Debug;
use std::sync::{Arc, Mutex, OnceLock};
use tokio::sync::{broadcast, watch as ready_watch};

/// Same namespace nodelet's own heartbeat Lease lives in
/// (`crates/nodelet/src/node.rs`'s `LEASE_NS`) — this is upstream's real
/// `NodeLease` mechanism, not a not-k8s invention: a per-node Lease renewed
/// cheaply and frequently (this project's `node-monitor-period=10s`) is what
/// node-lifecycle-controller actually watches for liveness upstream too,
/// not the much heavier full NodeStatus push.
pub const NODE_LEASE_NAMESPACE: &str = "kube-node-lease";

const WATCH_INITIAL_BACKOFF: std::time::Duration = std::time::Duration::from_millis(500);
const WATCH_MAX_BACKOFF: std::time::Duration = std::time::Duration::from_secs(30);

/// Use Kubernetes' streaming-list form for every controller watch. The
/// default ListWatch strategy performs a separate LIST and then a WATCH;
/// nodecontroller has many controllers (and several watch the same resource
/// kind), so starting them all that way creates a large, synchronized burst
/// of apiserver requests. Streaming lists carry the initial objects through
/// the watch itself, preserving the same Init/InitApply/InitDone sequence while
/// removing that extra request. This is also the path used by the reference
/// CSI provisioner in the real e2e setup, so the apiservers we support already
/// advertise the required feature.
fn watch_config() -> watcher::Config {
    watcher::Config::default().streaming_lists()
}

fn watch_backoff(consecutive_failures: u32) -> std::time::Duration {
    if consecutive_failures == 0 {
        return std::time::Duration::ZERO;
    }
    let doubled = WATCH_INITIAL_BACKOFF
        .checked_mul(1u32 << (consecutive_failures - 1).min(16))
        .unwrap_or(WATCH_MAX_BACKOFF);
    doubled.min(WATCH_MAX_BACKOFF)
}

#[derive(Default)]
struct WatchBackoffPolicy {
    consecutive_failures: u32,
}

impl Iterator for WatchBackoffPolicy {
    type Item = std::time::Duration;

    fn next(&mut self) -> Option<Self::Item> {
        self.consecutive_failures = self.consecutive_failures.saturating_add(1);
        Some(watch_backoff(self.consecutive_failures))
    }
}

impl Backoff for WatchBackoffPolicy {
    fn reset(&mut self) {
        self.consecutive_failures = 0;
    }
}

/// One apiserver watch shared by every controller that consumes the same
/// resource kind. Upstream controller-manager gets this property from its
/// shared informer factories; without it, every nodecontroller module that
/// needs Pods, Nodes, PVCs, and so on opens another long-lived watch. The
/// duplicate watches were harmless against a large apiserver but exhausted
/// the small k3s control plane's watch/concurrency budget in e2e, causing the
/// CSI provisioner's four watches to receive HTTP 429/Retry-After responses.
struct SharedWatch<T> {
    objects: Mutex<HashMap<String, T>>,
    ready: ready_watch::Sender<bool>,
    events: broadcast::Sender<Event<T>>,
}

impl<T> SharedWatch<T>
where
    T: Resource + ResourceExt + Clone + DeserializeOwned + Debug + Send + Sync + 'static,
{
    fn new(api: Api<T>) -> Arc<Self> {
        let (ready, _) = ready_watch::channel(false);
        let (events, _) = broadcast::channel(512);
        let shared = Arc::new(Self {
            objects: Mutex::new(HashMap::new()),
            ready,
            events,
        });

        let task_shared = shared.clone();
        tokio::spawn(async move {
            let mut stream = watcher(api, watch_config()).backoff(WatchBackoffPolicy::default());
            while let Some(result) = stream.next().await {
                let Ok(event) = result else {
                    tracing::warn!("shared nodecontroller watch received an error; kube-rs will retry it");
                    continue;
                };

                task_shared.apply(&event);
                let _ = task_shared.events.send(event);
            }
            tracing::warn!("shared nodecontroller watch ended");
        });

        shared
    }

    fn object_key(object: &T) -> String {
        format!("{}/{}", object.namespace().unwrap_or_default(), object.name_any())
    }

    fn apply(&self, event: &Event<T>) {
        match event {
            Event::Init => {
                self.objects.lock().expect("shared watch object store poisoned").clear();
                let _ = self.ready.send(false);
            }
            Event::InitApply(object) | Event::Apply(object) => {
                self.objects
                    .lock()
                    .expect("shared watch object store poisoned")
                    .insert(Self::object_key(object), object.clone());
            }
            Event::Delete(object) => {
                self.objects
                    .lock()
                    .expect("shared watch object store poisoned")
                    .remove(&Self::object_key(object));
            }
            Event::InitDone => {
                let _ = self.ready.send(true);
            }
        }
    }

    fn subscribe(self: &Arc<Self>) -> BoxStream<'static, watcher::Result<Event<T>>> {
        let subscription = SharedSubscription {
            shared: self.clone(),
            ready: self.ready.subscribe(),
            events: self.events.subscribe(),
            phase: SubscriptionPhase::NeedSnapshot,
            initial: Vec::new(),
            initial_index: 0,
        };
        futures::stream::unfold(subscription, |mut subscription| async move {
            subscription.next_event().await.map(|event| (Ok(event), subscription))
        })
        .boxed()
    }
}

#[derive(Clone, Copy)]
enum SubscriptionPhase {
    NeedSnapshot,
    Init,
    InitApply,
    InitDone,
    Live,
}

struct SharedSubscription<T>
where
    T: Resource + ResourceExt + Clone + DeserializeOwned + Debug + Send + Sync + 'static,
{
    shared: Arc<SharedWatch<T>>,
    ready: ready_watch::Receiver<bool>,
    events: broadcast::Receiver<Event<T>>,
    phase: SubscriptionPhase,
    initial: Vec<T>,
    initial_index: usize,
}

impl<T> SharedSubscription<T>
where
    T: Resource + ResourceExt + Clone + DeserializeOwned + Debug + Send + Sync + 'static,
{
    async fn next_event(&mut self) -> Option<Event<T>> {
        loop {
            match self.phase {
                SubscriptionPhase::NeedSnapshot => {
                    if !*self.ready.borrow() {
                        self.ready.changed().await.ok()?;
                        continue;
                    }

                    self.initial = self
                        .shared
                        .objects
                        .lock()
                        .expect("shared watch object store poisoned")
                        .values()
                        .cloned()
                        .collect();
                    self.initial_index = 0;
                    while self.events.try_recv().is_ok() {}
                    self.phase = SubscriptionPhase::Init;
                }
                SubscriptionPhase::Init => {
                    self.phase = if self.initial.is_empty() {
                        SubscriptionPhase::InitDone
                    } else {
                        SubscriptionPhase::InitApply
                    };
                    return Some(Event::Init);
                }
                SubscriptionPhase::InitApply => {
                    let object = self.initial.get(self.initial_index)?.clone();
                    self.initial_index += 1;
                    if self.initial_index == self.initial.len() {
                        self.phase = SubscriptionPhase::InitDone;
                    }
                    return Some(Event::InitApply(object));
                }
                SubscriptionPhase::InitDone => {
                    self.phase = SubscriptionPhase::Live;
                    return Some(Event::InitDone);
                }
                SubscriptionPhase::Live => match self.events.recv().await {
                    Ok(event) => return Some(event),
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        self.phase = SubscriptionPhase::NeedSnapshot;
                    }
                    Err(broadcast::error::RecvError::Closed) => return None,
                },
            }
        }
    }
}

macro_rules! shared_watch {
    ($name:ident, $static_name:ident, $resource:ty) => {
        pub fn $name(client: &Client) -> BoxStream<'static, watcher::Result<Event<$resource>>> {
            static $static_name: OnceLock<Arc<SharedWatch<$resource>>> = OnceLock::new();
            $static_name
                .get_or_init(|| SharedWatch::new(Api::all(client.clone())))
                .subscribe()
        }
    };
}

shared_watch!(watch_nodes, SHARED_NODES, Node);

pub fn watch_node_leases(client: &Client) -> BoxStream<'static, watcher::Result<Event<Lease>>> {
    let api: Api<Lease> = Api::namespaced(client.clone(), NODE_LEASE_NAMESPACE);
    watcher(api, watch_config()).backoff(WatchBackoffPolicy::default()).boxed()
}

shared_watch!(watch_namespaces, SHARED_NAMESPACES, Namespace);

shared_watch!(watch_services, SHARED_SERVICES, Service);

shared_watch!(watch_pods, SHARED_PODS, Pod);

shared_watch!(watch_resource_quotas, SHARED_RESOURCE_QUOTAS, ResourceQuota);

shared_watch!(watch_replica_sets, SHARED_REPLICA_SETS, ReplicaSet);

shared_watch!(watch_deployments, SHARED_DEPLOYMENTS, Deployment);

shared_watch!(watch_daemon_sets, SHARED_DAEMON_SETS, DaemonSet);

shared_watch!(watch_stateful_sets, SHARED_STATEFUL_SETS, StatefulSet);

shared_watch!(watch_jobs, SHARED_JOBS, Job);

shared_watch!(watch_cron_jobs, SHARED_CRON_JOBS, CronJob);

shared_watch!(watch_persistent_volume_claims, SHARED_PVCS, PersistentVolumeClaim);

shared_watch!(watch_persistent_volumes, SHARED_PVS, PersistentVolume);

shared_watch!(watch_volume_attachments, SHARED_VOLUME_ATTACHMENTS, VolumeAttachment);

shared_watch!(watch_certificate_signing_requests, SHARED_CSRS, CertificateSigningRequest);

shared_watch!(watch_pod_disruption_budgets, SHARED_PDBS, PodDisruptionBudget);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_failures_means_no_wait() {
        assert_eq!(watch_backoff(0), std::time::Duration::ZERO);
    }

    #[test]
    fn backoff_doubles_up_to_the_ceiling() {
        assert_eq!(watch_backoff(1), std::time::Duration::from_millis(500));
        assert_eq!(watch_backoff(2), std::time::Duration::from_millis(1000));
        assert_eq!(watch_backoff(3), std::time::Duration::from_millis(2000));
    }

    #[test]
    fn backoff_is_capped_and_does_not_overflow_on_many_failures() {
        assert_eq!(watch_backoff(100), WATCH_MAX_BACKOFF);
    }
}
