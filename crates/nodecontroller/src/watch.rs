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
use k8s_openapi::apiextensions_apiserver::pkg::apis::apiextensions::v1::CustomResourceDefinition;
use k8s_openapi::api::apps::v1::{DaemonSet, Deployment, ReplicaSet, StatefulSet};
use k8s_openapi::api::batch::v1::{CronJob, Job};
use k8s_openapi::api::certificates::v1::CertificateSigningRequest;
use k8s_openapi::api::coordination::v1::Lease;
use k8s_openapi::api::core::v1::{ConfigMap, Namespace, Node, PersistentVolume, PersistentVolumeClaim, Pod, ResourceQuota, Service, ServiceAccount};
use k8s_openapi::api::policy::v1::PodDisruptionBudget;
use k8s_openapi::api::storage::v1::{StorageClass, VolumeAttachment};
use kube::runtime::utils::{Backoff, WatchStreamExt};
use kube::runtime::watcher;
use kube::runtime::watcher::Event;
use kube::{Api, Client, Resource, ResourceExt};
use kube::api::{DynamicObject, TypeMeta};
use kube::core::{GroupVersionKind, PartialObjectMeta};
use kube::discovery::ApiResource;
use kube::discovery::Discovery;
use serde::de::DeserializeOwned;
use std::collections::HashMap;
use std::fmt::Debug;
use std::sync::{Arc, Mutex, OnceLock};
use tokio::sync::{broadcast, watch as ready_watch, Semaphore};

/// Same namespace nodelet's own heartbeat Lease lives in
/// (`crates/nodelet/src/node.rs`'s `LEASE_NS`) — this is upstream's real
/// `NodeLease` mechanism, not a not-k8s invention: a per-node Lease renewed
/// cheaply and frequently (this project's `node-monitor-period=10s`) is what
/// node-lifecycle-controller actually watches for liveness upstream too,
/// not the much heavier full NodeStatus push.
pub const NODE_LEASE_NAMESPACE: &str = "kube-node-lease";

const WATCH_INITIAL_BACKOFF: std::time::Duration = std::time::Duration::from_secs(1);
const WATCH_MAX_BACKOFF: std::time::Duration = std::time::Duration::from_secs(30);

/// Shared informer startup admission. An initial LIST can make the apiserver
/// do substantial work. Keeping only a small number of snapshots in flight
/// prevents all controller domains from becoming ready in one synchronized
/// burst. The permit is released at InitDone, so this does not serialize
/// steady-state watches or event delivery.
static WATCH_STARTUP_SEMAPHORE: OnceLock<Arc<Semaphore>> = OnceLock::new();

pub fn configure_startup_concurrency(limit: usize) {
    let _ = WATCH_STARTUP_SEMAPHORE.set(Arc::new(Semaphore::new(limit.max(1))));
}

fn startup_semaphore() -> Arc<Semaphore> {
    WATCH_STARTUP_SEMAPHORE
        .get_or_init(|| Arc::new(Semaphore::new(2)))
        .clone()
}

/// Use the ordinary LIST-then-WATCH strategy for controller informers. A
/// streaming list saves one request, but its long-running watch-list request
/// also competes for the small apiserver's long-running request seats during
/// startup. The bounded semaphore already spaces the short initial LISTs, and
/// the normal watch is established only after its snapshot has completed. The
/// shared subscription still exposes the same Init/InitApply/InitDone API to
/// controllers.
fn watch_config() -> watcher::Config {
    watcher::Config::default()
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
        Self::with_startup_semaphore(api, startup_semaphore())
    }

    fn with_startup_semaphore(api: Api<T>, startup: Arc<Semaphore>) -> Arc<Self> {
        let (ready, _) = ready_watch::channel(false);
        let (events, _) = broadcast::channel(512);
        let shared = Arc::new(Self {
            objects: Mutex::new(HashMap::new()),
            ready,
            events,
        });

        let task_shared = shared.clone();
        tokio::spawn(async move {
            let mut initialized = false;
            let mut initial_failures: u32 = 0;
            loop {
                let mut startup_permit = if initialized { None } else {
                    Some(startup.clone().acquire_owned().await
                        .expect("nodecontroller watch startup semaphore was closed"))
                };
                if !initialized {
                    tracing::info!(resource = %std::any::type_name::<T>(), "shared informer starting initial LIST");
                }
                let mut stream = Box::pin(watcher(api.clone(), watch_config()).backoff(WatchBackoffPolicy::default()));
                let mut listing = true;
                loop {
                    // Every new stream starts with a LIST, even after startup.
                    // Bound a stalled page, not a healthy idle watch.
                    // On failure release admission before sleeping/retrying so
                    // one unavailable kind cannot park every later informer.
                    let next = if !listing { stream.next().await } else {
                        // Start with a one-second request budget, then allow
                        // slower snapshots progressively more time (max 30s).
                        let deadline = watch_backoff(initial_failures.saturating_add(1));
                        match tokio::time::timeout(deadline, stream.next()).await {
                            Ok(next) => next,
                            Err(_) => {
                                tracing::warn!(resource = %std::any::type_name::<T>(), "shared informer LIST stalled; releasing any startup slot");
                                break;
                            }
                        }
                    };
                    let Some(result) = next else { break };
                    let event = match result {
                        Ok(event) => event,
                        Err(error) => {
                            tracing::warn!(resource = %std::any::type_name::<T>(), error = ?error,
                                "shared nodecontroller watch received an error; restarting with a bounded LIST");
                            break;
                        }
                    };

                    let initial_done = matches!(&event, Event::InitDone);
                    if matches!(&event, Event::Init) { listing = true; }
                    if initial_done {
                        listing = false;
                        initial_failures = 0;
                    }
                    task_shared.apply(&event);
                    if initial_done && startup_permit.is_some() {
                        // Only the initial snapshot is admission-controlled.
                        // Steady-state watches remain independent.
                        drop(startup_permit.take());
                        initialized = true;
                        initial_failures = 0;
                        tracing::info!(resource = %std::any::type_name::<T>(), "shared informer installed initial snapshot");
                    }
                }
                // Cancel any pending request before admitting another LIST.
                drop(stream);
                drop(startup_permit);
                tracing::warn!(resource = %std::any::type_name::<T>(), "shared nodecontroller watch ended; restarting");
                initial_failures = initial_failures.saturating_add(1);
                tokio::time::sleep(watch_backoff(initial_failures)).await;
            }
        });

        shared
    }

    fn object_key(object: &T) -> String {
        format!("{}/{}", object.namespace().unwrap_or_default(), object.name_any())
    }

    fn apply(&self, event: &Event<T>) {
        if std::any::TypeId::of::<T>() == std::any::TypeId::of::<Namespace>() {
            match event {
                Event::Apply(object) | Event::InitApply(object) | Event::Delete(object) => {
                    tracing::debug!(target: "nk_watch_trace", boundary = "http_to_informer",
                        name = %object.name_any(), revision = ?object.resource_version(),
                        deleted = matches!(event, Event::Delete(_)), "namespace watch event");
                }
                _ => {}
            }
        }
        // Publish under the snapshot lock. A subscriber must see an event
        // either in its snapshot or in its new receiver, never an older
        // buffered event replayed after a newer snapshot.
        let mut objects = self.objects.lock().expect("shared watch object store poisoned");
        match event {
            Event::Init => {
                objects.clear();
                self.ready.send_replace(false);
            }
            Event::InitApply(object) | Event::Apply(object) => {
                objects.insert(Self::object_key(object), object.clone());
            }
            Event::Delete(object) => {
                objects.remove(&Self::object_key(object));
            }
            Event::InitDone => {
                self.ready.send_replace(true);
            }
        }
        let _ = self.events.send(event.clone());
    }

    fn snapshot_and_subscribe(&self) -> Option<(Vec<T>, broadcast::Receiver<Event<T>>)> {
        let objects = self.objects.lock().expect("shared watch object store poisoned");
        if !*self.ready.borrow() {
            return None;
        }
        Some((objects.values().cloned().collect(), self.events.subscribe()))
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
                    let Some((initial, events)) = self.shared.snapshot_and_subscribe() else {
                        self.ready.changed().await.ok()?;
                        continue;
                    };
                    self.initial = initial;
                    self.events = events;
                    self.initial_index = 0;
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

fn as_dynamic<T>(object: T) -> DynamicObject
where
    T: Resource<DynamicType = ()> + ResourceExt,
{
    DynamicObject {
        types: Some(TypeMeta {
            api_version: T::api_version(&()).to_string(),
            kind: T::kind(&()).to_string(),
        }),
        metadata: object.meta().clone(),
        data: serde_json::Value::Null,
    }
}

fn dynamic_event<T>(event: Event<T>) -> Event<DynamicObject>
where
    T: Resource<DynamicType = ()> + ResourceExt,
{
    match event {
        Event::Apply(object) => Event::Apply(as_dynamic(object)),
        Event::Delete(object) => Event::Delete(as_dynamic(object)),
        Event::Init => Event::Init,
        Event::InitApply(object) => Event::InitApply(as_dynamic(object)),
        Event::InitDone => Event::InitDone,
    }
}

/// The identity every shared/dedup'd read in this file actually connects
/// as -- real upstream `kube-controller-manager` backs its shared informer
/// factory with the base client (`system:kube-controller-manager`'s own
/// built-in bootstrap role, which is deliberately broad on reads: it's
/// what the shared informers run as), not any single controller's
/// narrowly-scoped per-SA impersonated identity. Set once, early, by
/// `lib.rs::run()` before any controller starts. Every function below
/// still takes its own `client: &Client` parameter for source
/// compatibility with every existing call site (~20 controller modules)
/// but deliberately ignores it in favor of this one -- see
/// `docs/E2E_FINDINGS.md` finding 22's follow-up for why: a per-controller
/// impersonated client doesn't mean anything for a *shared* watch anyway
/// (`SharedWatch`'s `OnceLock` means only the first caller's client
/// argument would ever have mattered, which is racy and not what any
/// caller intends), and real upstream's own per-controller bootstrap
/// roles (e.g. `system:controller:node-controller`) confirm reads like
/// this were never meant to be covered by them -- `node-controller`'s own
/// role has no `coordination.k8s.io` `leases` rule at all, despite
/// `node-lifecycle-controller` needing to watch them.
static BASE_CLIENT: OnceLock<Client> = OnceLock::new();

/// Called once by `lib.rs::run()`, before any controller starts.
pub fn set_base_client(client: Client) {
    let _ = BASE_CLIENT.set(client);
}

pub(crate) fn base_client() -> Client {
    BASE_CLIENT.get().expect("watch::set_base_client() must be called before any watch starts").clone()
}

macro_rules! shared_watch {
    ($name:ident, $static_name:ident, $resource:ty) => {
        pub fn $name(_client: &Client) -> BoxStream<'static, watcher::Result<Event<$resource>>> {
            static $static_name: OnceLock<Arc<SharedWatch<$resource>>> = OnceLock::new();
            $static_name
                .get_or_init(|| SharedWatch::new(Api::all(base_client())))
                .subscribe()
        }
    };
}

shared_watch!(watch_nodes, SHARED_NODES, Node);

pub fn watch_node_leases(_client: &Client) -> BoxStream<'static, watcher::Result<Event<Lease>>> {
    let api: Api<Lease> = Api::namespaced(base_client(), NODE_LEASE_NAMESPACE);
    watcher(api, watch_config()).backoff(WatchBackoffPolicy::default()).boxed()
}

shared_watch!(watch_namespaces, SHARED_NAMESPACES, Namespace);

// Every controller that caches API discovery subscribes to this one
// informer. A CRD changes the set of API resources served by the apiserver;
// keeping that signal shared avoids one independent CRD watch per consumer
// while still letting namespace cleanup and garbage collection refresh their
// own discovery cache when a CRD is installed or removed.
shared_watch!(
    watch_custom_resource_definitions,
    SHARED_CUSTOM_RESOURCE_DEFINITIONS,
    CustomResourceDefinition
);

shared_watch!(watch_service_accounts, SHARED_SERVICE_ACCOUNTS, ServiceAccount);

shared_watch!(watch_config_maps, SHARED_CONFIG_MAPS, ConfigMap);

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

/// PVC consumers that only need existence and ownership must not deserialize
/// the PVC spec/status on every CSI provisioning or status update. Keep this
/// informer metadata-only; the API request's negotiated
/// `PartialObjectMetadata` representation excludes the rest of the object.
pub fn watch_persistent_volume_claim_metadata(
    _client: &Client,
) -> BoxStream<'static, watcher::Result<Event<PartialObjectMeta<PersistentVolumeClaim>>>> {
    let api: Api<PartialObjectMeta<PersistentVolumeClaim>> = Api::all(base_client());
    // A busy k3s apiserver can return a raw Status body while a metadata watch
    // is being re-established. kube-rs reports that as WatchFailed and keeps
    // the underlying stream in place, so applying the ordinary backoff alone
    // would repeatedly poll the same poisoned stream forever. Recreate the
    // watcher after every error; the request remains metadata-only, and the
    // controller below still decides whether a newly decoded metadata object
    // is meaningful before replacing its cache entry.
    type MetadataEvent = watcher::Result<Event<PartialObjectMeta<PersistentVolumeClaim>>>;
    type MetadataStream = BoxStream<'static, MetadataEvent>;

    futures::stream::unfold(
        (
            api,
            None::<MetadataStream>,
            WatchBackoffPolicy::default(),
            None::<std::time::Duration>,
        ),
        |(api, mut stream, mut backoff, delay)| async move {
            if let Some(delay) = delay {
                tokio::time::sleep(delay).await;
            }

            loop {
                if stream.is_none() {
                    stream = Some(watcher(api.clone(), watch_config()).boxed());
                }
                let next = stream
                    .as_mut()
                    .expect("metadata watcher stream was just initialized")
                    .next()
                    .await;

                match next {
                    Some(Ok(event)) => {
                        backoff.reset();
                        return Some((Ok(event), (api, stream, backoff, None)));
                    }
                    Some(Err(error)) => {
                        let delay = backoff.next().unwrap_or(WATCH_MAX_BACKOFF);
                        stream = None;
                        return Some((Err(error), (api, stream, backoff, Some(delay))));
                    }
                    None => {
                        let delay = backoff.next().unwrap_or(WATCH_MAX_BACKOFF);
                        stream = None;
                        tokio::time::sleep(delay).await;
                    }
                }
            }
        },
    )
    .boxed()
}

shared_watch!(watch_persistent_volumes, SHARED_PVS, PersistentVolume);

shared_watch!(watch_storage_classes, SHARED_STORAGE_CLASSES, StorageClass);

shared_watch!(watch_volume_attachments, SHARED_VOLUME_ATTACHMENTS, VolumeAttachment);

shared_watch!(watch_certificate_signing_requests, SHARED_CSRS, CertificateSigningRequest);

shared_watch!(watch_pod_disruption_budgets, SHARED_PDBS, PodDisruptionBudget);

/// Run API discovery, retrying when the apiserver is temporarily unavailable
/// or is still publishing a newly-created CRD. Kubernetes updates a CRD and
/// its served group/version in separate steps, so a refresh triggered by the
/// CRD informer must tolerate that short propagation window.
pub async fn discover_api(client: &Client, controller: &'static str) -> Discovery {
    let mut delay = std::time::Duration::from_secs(1);
    loop {
        match Discovery::new((*client).clone()).run().await {
            Ok(discovery) => return discovery,
            Err(error) => {
                tracing::warn!(
                    controller,
                    error = ?error,
                    retry_in = ?delay,
                    "nodecontroller API discovery failed; retrying"
                );
                tokio::time::sleep(delay).await;
                delay = delay.saturating_mul(2).min(WATCH_MAX_BACKOFF);
            }
        }
    }
}

pub fn watch_resource_claim_templates(
    _client: &Client,
) -> BoxStream<'static, watcher::Result<Event<DynamicObject>>> {
    let gvk = GroupVersionKind::gvk("resource.k8s.io", "v1", "ResourceClaimTemplate");
    let resource = ApiResource::from_gvk(&gvk);
    let api: Api<DynamicObject> = Api::all_with(base_client(), &resource);
    watcher(api, watch_config()).backoff(WatchBackoffPolicy::default()).boxed()
}

/// Watch a discovered resource through its metadata representation. Discovery
/// can expose many CRD kinds, so these streams must use the same start-failure
/// backoff as the shared typed informers. Without it, an apiserver restart or
/// a relist after HTTP 410 makes every GC stream immediately retry its LIST,
/// creating a request storm that starves the ordinary controllers.
pub fn watch_dynamic_metadata_resource(
    client: &Client,
    resource: &ApiResource,
) -> BoxStream<'static, watcher::Result<Event<PartialObjectMeta<DynamicObject>>>> {
    let api: Api<PartialObjectMeta<DynamicObject>> = Api::all_with(client.clone(), resource);
    watcher(api, watch_config())
        .backoff(WatchBackoffPolicy::default())
        .boxed()
}

/// Return the shared typed watch for a built-in namespaced resource in the
/// shape the generic garbage collector consumes. Keeping this conversion
/// here lets GC reuse the same underlying watch as the typed controllers
/// instead of opening a second dynamic watch for the same GVK.
pub fn watch_dynamic_resource(
    client: &Client,
    api_version: &str,
    kind: &str,
) -> Option<BoxStream<'static, watcher::Result<Event<DynamicObject>>>> {
    macro_rules! dynamic_shared {
        ($watch:ident) => {
            Some($watch(client).map(|event| event.map(dynamic_event)).boxed())
        };
    }

    match (api_version, kind) {
        ("v1", "Pod") => dynamic_shared!(watch_pods),
        ("v1", "PersistentVolumeClaim") => dynamic_shared!(watch_persistent_volume_claims),
        ("v1", "ResourceQuota") => dynamic_shared!(watch_resource_quotas),
        ("v1", "Service") => dynamic_shared!(watch_services),
        ("apps/v1", "DaemonSet") => dynamic_shared!(watch_daemon_sets),
        ("apps/v1", "Deployment") => dynamic_shared!(watch_deployments),
        ("apps/v1", "ReplicaSet") => dynamic_shared!(watch_replica_sets),
        ("apps/v1", "StatefulSet") => dynamic_shared!(watch_stateful_sets),
        ("batch/v1", "CronJob") => dynamic_shared!(watch_cron_jobs),
        ("batch/v1", "Job") => dynamic_shared!(watch_jobs),
        ("policy/v1", "PodDisruptionBudget") => dynamic_shared!(watch_pod_disruption_budgets),
        ("storage.k8s.io/v1", "StorageClass") => dynamic_shared!(watch_storage_classes),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn snapshot_discards_lagged_events_before_following_live_updates() {
        let (ready, _) = ready_watch::channel(false);
        let (events, _) = broadcast::channel(2);
        let shared = Arc::new(SharedWatch::<Namespace> {
            objects: Mutex::new(HashMap::new()), ready, events,
        });
        shared.apply(&Event::InitDone);
        let mut subscription = shared.subscribe();
        let mut old = Namespace::default();
        old.metadata.name = Some("replaced".into());
        old.metadata.uid = Some("old".into());
        shared.apply(&Event::Apply(old.clone()));
        shared.apply(&Event::Delete(old));
        let mut replacement = Namespace::default();
        replacement.metadata.name = Some("replaced".into());
        replacement.metadata.uid = Some("new".into());
        shared.apply(&Event::Apply(replacement.clone()));
        // The receiver is now lagged. Draining with `while try_recv().is_ok()`
        // stops at Lagged and leaves the stale deletion behind the snapshot.
        assert!(matches!(subscription.next().await, Some(Ok(Event::Init))));
        assert!(matches!(subscription.next().await,
            Some(Ok(Event::InitApply(ns))) if ns.uid().as_deref() == Some("new")));
        assert!(matches!(subscription.next().await, Some(Ok(Event::InitDone))));
        replacement.metadata.resource_version = Some("4".into());
        shared.apply(&Event::Apply(replacement));
        assert!(matches!(subscription.next().await,
            Some(Ok(Event::Apply(ns))) if ns.resource_version().as_deref() == Some("4")));
    }

    #[test]
    fn snapshot_waits_for_the_complete_relist_and_subscribes_at_its_boundary() {
        let (ready, _) = ready_watch::channel(false);
        let (events, _) = broadcast::channel(8);
        let shared = SharedWatch::<Namespace> {
            objects: Mutex::new(HashMap::new()), ready, events,
        };
        shared.apply(&Event::Init);
        let mut ns = Namespace::default();
        ns.metadata.name = Some("listed".into());
        shared.apply(&Event::InitApply(ns.clone()));
        assert!(shared.snapshot_and_subscribe().is_none());
        shared.apply(&Event::InitDone);
        let (snapshot, mut events) = shared.snapshot_and_subscribe().unwrap();
        assert_eq!(snapshot.len(), 1);
        assert!(matches!(events.try_recv(), Err(broadcast::error::TryRecvError::Empty)));
        shared.apply(&Event::Delete(ns));
        assert!(matches!(events.try_recv(), Ok(Event::Delete(_))));
    }

    #[tokio::test(start_paused = true)]
    async fn stalled_reconnect_list_is_retried_without_startup_admission() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let lists = Arc::new(AtomicUsize::new(0));
        let requests = lists.clone();
        let reconnecting = Arc::new(tokio::sync::Notify::new());
        let observed = reconnecting.clone();
        let service = tower::service_fn(move |request: http::Request<kube::client::Body>| {
            let lists = requests.clone();
            let reconnecting = reconnecting.clone();
            async move {
                if request.uri().query().is_some_and(|query| query.contains("watch=true")) {
                    if lists.load(Ordering::SeqCst) == 1 {
                        return Ok::<_, std::convert::Infallible>(http::Response::builder()
                            .status(410).body(kube::client::Body::from(
                                br#"{"apiVersion":"v1","kind":"Status","status":"Failure","reason":"Expired","message":"relist","code":410}"#.to_vec()
                            )).unwrap());
                    }
                    futures::future::pending::<()>().await;
                }
                let number = lists.fetch_add(1, Ordering::SeqCst) + 1;
                if number == 2 {
                    reconnecting.notify_one();
                    futures::future::pending::<()>().await;
                }
                let name = if number == 1 { "before" } else { "after" };
                Ok(http::Response::new(kube::client::Body::from(serde_json::to_vec(
                    &serde_json::json!({"apiVersion":"v1","kind":"PodList",
                        "metadata":{"resourceVersion":number.to_string()},
                        "items":[{"apiVersion":"v1","kind":"Pod","metadata":{"name":name}}]})
                ).unwrap())))
            }
        });
        let admission = Arc::new(Semaphore::new(1));
        let shared = SharedWatch::<Pod>::with_startup_semaphore(
            Api::all(Client::new(service, "default")), admission.clone());
        tokio::time::timeout(std::time::Duration::from_secs(10), observed.notified())
            .await.expect("the watch must enter its second LIST");
        // Reconnects must not compete for initial startup slots.
        let _held = admission.try_acquire().expect("initial snapshot released admission");
        let mut subscription = shared.subscribe();
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            while let Some(Ok(event)) = subscription.next().await {
                if matches!(event, Event::InitApply(ref pod) if pod.name_any() == "after") {
                    return;
                }
            }
            panic!("subscription ended before the replacement snapshot");
        }).await.expect("a stalled reconnect LIST must time out and recover");
        assert_eq!(lists.load(Ordering::SeqCst), 3);
        tokio::time::sleep(WATCH_MAX_BACKOFF * 2).await;
        assert_eq!(lists.load(Ordering::SeqCst), 3, "idle watches must not use the LIST deadline");
    }

    #[tokio::test(start_paused = true)]
    async fn stalled_initial_list_releases_admission_before_retry_backoff() {
        let entered = Arc::new(tokio::sync::Notify::new());
        let observed = entered.clone();
        let service = tower::service_fn(move |request: http::Request<kube::client::Body>| {
            let entered = entered.clone();
            async move {
                if request.uri().path().ends_with("/namespaces") {
                    entered.notify_one();
                    futures::future::pending::<()>().await;
                }
                if request.uri().query().is_some_and(|query| query.contains("watch=true")) {
                    futures::future::pending::<()>().await;
                }
                Ok::<_, std::convert::Infallible>(http::Response::new(kube::client::Body::from(
                    br#"{"apiVersion":"v1","kind":"PodList","metadata":{"resourceVersion":"1"},"items":[]}"#.to_vec()
                )))
            }
        });
        let client = Client::new(service, "default");
        let admission = Arc::new(Semaphore::new(1));
        let _stalled = SharedWatch::<Namespace>::with_startup_semaphore(Api::all(client.clone()), admission.clone());
        observed.notified().await; // Prove the first request owns the only slot.
        let healthy = SharedWatch::<Pod>::with_startup_semaphore(Api::all(client), admission);
        let started = tokio::time::Instant::now();
        let mut subscription = healthy.subscribe();
        tokio::time::timeout(WATCH_INITIAL_BACKOFF * 2, async {
            assert!(matches!(subscription.next().await, Some(Ok(Event::Init))));
            assert!(matches!(subscription.next().await, Some(Ok(Event::InitDone))));
        }).await.expect("a failed kind must release admission before its retry");
        assert_eq!(started.elapsed(), WATCH_INITIAL_BACKOFF);
    }

    #[tokio::test]
    async fn a_late_subscriber_receives_the_completed_snapshot() {
        let (ready, _) = ready_watch::channel(false);
        let (events, _) = broadcast::channel(2);
        let shared = Arc::new(SharedWatch::<Namespace> {
            objects: Mutex::new(HashMap::new()), ready, events,
        });
        let mut namespace = Namespace::default();
        namespace.metadata.name = Some("after-restart".into());
        shared.apply(&Event::Init);
        shared.apply(&Event::InitApply(namespace));
        // There are no subscribers yet. send() would lose this readiness
        // value, parking a subsequent GC subscription forever.
        shared.apply(&Event::InitDone);
        let mut subscription = shared.subscribe();
        let events = tokio::time::timeout(std::time::Duration::from_secs(1), async {
            let mut events = Vec::new();
            for _ in 0..3 { events.push(subscription.next().await.unwrap().unwrap()); }
            events
        }).await.expect("late subscribers must not wait for another relist");
        assert!(matches!(&events[0], Event::Init));
        assert!(matches!(&events[1], Event::InitApply(ns) if ns.name_any() == "after-restart"));
        assert!(matches!(&events[2], Event::InitDone));
    }

    #[test]
    fn no_failures_means_no_wait() {
        assert_eq!(watch_backoff(0), std::time::Duration::ZERO);
    }

    #[test]
    fn backoff_doubles_up_to_the_ceiling() {
        let mut policy = WatchBackoffPolicy::default();
        for seconds in [1, 2, 4, 8, 16, 30, 30] {
            assert_eq!(policy.next(), Some(std::time::Duration::from_secs(seconds)));
        }
        policy.reset();
        assert_eq!(policy.next(), Some(WATCH_INITIAL_BACKOFF));
    }

    #[test]
    fn backoff_is_capped_and_does_not_overflow_on_many_failures() {
        assert_eq!(watch_backoff(100), WATCH_MAX_BACKOFF);
    }
}
