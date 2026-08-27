//! garbage-collector-controller (Group D): owner-reference cascade
//! deletion, generic across every namespaced, watchable, deletable
//! resource kind the apiserver serves — discovered from the live API surface,
//! not a hardcoded list of the kinds this crate happens to already know about.
//! The deferral this file closes: `docs/CONTROLLER_MANAGER.md`'s Group D
//! section explains why this waited for Group E to exist first (nothing
//! produced a real owner chain — Deployment→ReplicaSet→Pod — worth
//! cleaning up before then).
//!
//! # How it works (event-driven, no polling, no explicit graph walk)
//!
//! One watch per discovered resource kind, all funneled into a single event
//! loop that tracks two things purely from watch events: which
//! UIDs currently exist, and — for every object that *has* an
//! `ownerReference` — a reverse index from each owner's UID to its
//! children. When an owner's Delete event arrives, every child in its
//! reverse-index entry whose *entire* owner list is now dead gets deleted
//! immediately (background propagation — see below). That child's own
//! subsequent Delete event (once the apiserver processes it) is itself an
//! event through this same loop, so a grandchild gets cleaned up as a
//! natural consequence of the child's deletion being observed — recursion
//! falls out of the event loop itself, no recursive function needed.
//!
//! Also handles the "owner was already gone before the child was ever
//! observed" case (a relist redelivering a child as `InitApply` after its
//! dead owner has long since been swept elsewhere): once every discovered
//! kind's *own* initial list has completed at least once (tracked per
//! kind, all-or-nothing before this controller acts on anything), any
//! object whose owners are *all* already known-dead is deleted right away
//! instead of only reacting to a live Delete event.
//!
//! # Scope of this slice
//!
//! **Discovery is refreshed by a shared CRD informer.** A CRD installed after
//! nodecontroller starts causes the current generation of dynamic watches to
//! be replaced with a generation built from fresh discovery, matching the
//! important live behavior of upstream's invalidatable RESTMapper.
//!
//! **Namespaced resources only.** An owner reference is same-namespace by
//! the API's own rule (`OwnerReference` carries no namespace field, so
//! cross-namespace ownership isn't representable at all) — a cluster-scoped
//! resource (a Node, a ClusterRole, a PersistentVolume) is never a valid GC
//! *target*, so this controller never watches cluster-scoped kinds at all,
//! not even as a potential owner. This matches upstream's own real scope,
//! not a simplification.
//!
//! **Background propagation only.** Every cascade delete is immediate,
//! matching upstream's modern default. `propagationPolicy: Foreground`
//! (block the parent's own deletion behind a finalizer until every child is
//! gone first) and `Orphan` (strip owner references instead of deleting)
//! are not implemented — every deletion here behaves as `Background`
//! regardless of what a caller's `DeleteOptions.propagationPolicy` asked
//! for. A real, occasionally-felt difference (an `Orphan`-requested delete
//! still cascades here) worth naming plainly.
//!
//! **A fixed set of high-churn/GC-irrelevant groups is excluded**
//! (`coordination.k8s.io` — Lease, renewed every few seconds by nodelet's
//! own heartbeat and utterly unrelated to owner-reference cleanup;
//! `events.k8s.io` and the core `Event` kind — high-volume, ownerReferences
//! point *from* an Event *at* its subject but nothing ever needs to cascade
//! delete an Event). Every other namespaced, watchable, deletable kind —
//! built-in or CRD — is covered generically.

use anyhow::Result;
use futures::stream::{select_all, BoxStream, StreamExt};
use k8s_openapi::apiextensions_apiserver::pkg::apis::apiextensions::v1::CustomResourceDefinition;
use kube::api::{Api, DeleteParams, DynamicObject, Preconditions, PropagationPolicy};
use kube::core::PartialObjectMeta;
use kube::discovery::{verbs, Discovery, Scope};
use kube::runtime::watcher;
use kube::runtime::watcher::Event;
use kube::{Client, ResourceExt};
use std::collections::{HashMap, HashSet};

const EXCLUDED_GROUPS: &[&str] = &["coordination.k8s.io", "events.k8s.io"];
const EXCLUDED_KINDS: &[&str] = &["Event"];

fn should_watch(
    ar: &kube::discovery::ApiResource,
    caps: &kube::discovery::ApiCapabilities,
) -> bool {
    caps.scope == Scope::Namespaced
        && caps.supports_operation(verbs::WATCH)
        && caps.supports_operation(verbs::LIST)
        && caps.supports_operation(verbs::DELETE)
        && !EXCLUDED_GROUPS.contains(&ar.group.as_str())
        && !EXCLUDED_KINDS.contains(&ar.kind.as_str())
}

fn gvk_key(ar: &kube::discovery::ApiResource) -> String {
    format!("{}/{}", ar.api_version, ar.kind)
}

/// `State`'s own bookkeeping (`handle_apply`/`handle_delete`/`owner_uids_of`)
/// only ever reads `.metadata` off a `DynamicObject` — never `.data`, the
/// `#[serde(flatten)]`'d rest of the object (spec/status/Secret/ConfigMap
/// payloads, ...). For a kind with no shared typed watch to piggyback on
/// (see `run()`'s own comment), that flattened field is otherwise a real,
/// paid-for cost with nothing to show for it: it forces every LIST/WATCH
/// response for that kind to build a full `serde_json::Value` tree just to
/// be thrown away. Real profiling on issue #40 found exactly this shape —
/// ~28% of sampled idle CPU inside `serde_json` call stacks tied to this
/// controller's own dynamic watches. `PartialObjectMeta<DynamicObject>`
/// makes `Api`/`watcher` negotiate the apiserver's metadata-only response
/// (`Accept: application/json;as=PartialObjectMetadata;...`) automatically
/// — the same `metadataInformer` mechanism upstream's real garbage
/// collector uses for exactly this reason — so the flattened body is never
/// sent by the apiserver, let alone deserialized here. `data: Value::Null`
/// mirrors `watch::as_dynamic()`'s own placeholder for the shared-watch
/// case, keeping `State`'s methods unchanged either way.
fn from_partial_metadata(partial: PartialObjectMeta<DynamicObject>) -> DynamicObject {
    DynamicObject {
        types: partial.types,
        metadata: partial.metadata,
        data: serde_json::Value::Null,
    }
}

/// `Event<K>` has no `map`-to-a-different-`K` method of its own (its real
/// `modify()` only mutates a value in place, same type in and out) — this
/// is the per-variant conversion `Event<PartialObjectMeta<DynamicObject>>`
/// -> `Event<DynamicObject>` needs instead, applying [`from_partial_metadata`]
/// to whichever variant actually carries an object.
fn map_partial_metadata_event(event: Event<PartialObjectMeta<DynamicObject>>) -> Event<DynamicObject> {
    match event {
        Event::Apply(obj) => Event::Apply(from_partial_metadata(obj)),
        Event::Delete(obj) => Event::Delete(from_partial_metadata(obj)),
        Event::InitApply(obj) => Event::InitApply(from_partial_metadata(obj)),
        Event::Init => Event::Init,
        Event::InitDone => Event::InitDone,
    }
}

#[derive(Debug, Clone)]
struct ObjRecord {
    uid: String,
    gvk_key: String,
    namespace: String,
    name: String,
    owner_uids: Vec<String>,
    deleting: bool,
}

fn owner_uids_of(obj: &DynamicObject) -> Vec<String> {
    obj.metadata
        .owner_references
        .as_ref()
        .into_iter()
        .flatten()
        .filter(|o| !o.uid.is_empty())
        .map(|o| o.uid.clone())
        .collect()
}

/// An object with no owner references is not an orphan. `Iterator::all()` is
/// true for an empty iterator, so keeping the non-empty check here is
/// important: otherwise every ordinary namespace-scoped object (including
/// ServiceAccounts and the per-namespace `kube-root-ca.crt` ConfigMap) looks
/// orphaned as soon as the initial relist completes.
fn all_owners_dead(owner_uids: &[String], exists: &HashSet<String>) -> bool {
    !owner_uids.is_empty() && owner_uids.iter().all(|owner| !exists.contains(owner))
}

fn should_delete_orphan(record: &ObjRecord, ready: bool, exists: &HashSet<String>) -> bool {
    ready && !record.deleting && all_owners_dead(&record.owner_uids, exists)
}

/// Deletes `record`, background-propagated. Silently ignores "already
/// gone" — the routine outcome of two cascade paths reaching the same
/// child (e.g. discovered both directly and via a since-vanished owner).
async fn delete_object(
    client: &Client,
    resources: &HashMap<String, kube::discovery::ApiResource>,
    record: &ObjRecord,
) {
    let Some(ar) = resources.get(&record.gvk_key) else {
        return;
    };
    let api: Api<DynamicObject> = Api::namespaced_with(client.clone(), &record.namespace, ar);
    let dp = DeleteParams {
        propagation_policy: Some(PropagationPolicy::Background),
        preconditions: Some(Preconditions {
            uid: Some(record.uid.clone()),
            resource_version: None,
        }),
        ..Default::default()
    };
    match api.delete(&record.name, &dp).await {
        Ok(_) => {
            tracing::info!(kind = %record.gvk_key, namespace = %record.namespace, name = %record.name, "garbage-collector-controller deleted an orphaned object")
        }
        Err(kube::Error::Api(ref status)) if status.is_not_found() || status.code == 409 => {}
        Err(e) => {
            tracing::warn!(kind = %record.gvk_key, namespace = %record.namespace, name = %record.name, error = ?e, "garbage-collector-controller failed to delete an orphaned object")
        }
    }
}

struct State {
    resources: HashMap<String, kube::discovery::ApiResource>,
    exists: HashSet<String>,
    objects_with_owners: HashMap<String, ObjRecord>,
    children_of: HashMap<String, HashSet<String>>,
    pending_init: HashSet<String>,
    uid_to_kind: HashMap<String, String>,
    relist: HashMap<String, HashMap<String, ObjRecord>>,
}

impl State {
    fn ready(&self) -> bool {
        self.pending_init.is_empty()
    }

    fn store_record(&mut self, record: ObjRecord) {
        let uid = record.uid.clone();
        self.exists.insert(uid.clone());
        self.uid_to_kind.insert(uid.clone(), record.gvk_key.clone());

        if let Some(old) = self.objects_with_owners.get(&uid).cloned() {
            for old_owner in &old.owner_uids {
                if !record.owner_uids.contains(old_owner) {
                    if let Some(set) = self.children_of.get_mut(old_owner) {
                        set.remove(&uid);
                    }
                }
            }
        }
        if record.owner_uids.is_empty() {
            self.objects_with_owners.remove(&uid);
            return;
        }
        for owner in &record.owner_uids {
            self.children_of
                .entry(owner.clone())
                .or_default()
                .insert(uid.clone());
        }
        self.objects_with_owners.insert(uid, record);
    }

    async fn handle_apply(
        &mut self,
        client: &Client,
        kind_key: &str,
        obj: DynamicObject,
        staged: bool,
    ) {
        let Some(uid) = obj.uid() else { return };
        let owner_uids = owner_uids_of(&obj);
        let record = ObjRecord {
            uid: uid.clone(),
            gvk_key: kind_key.to_string(),
            namespace: obj.namespace().unwrap_or_default(),
            name: obj.name_any(),
            owner_uids: owner_uids.clone(),
            deleting: obj.metadata.deletion_timestamp.is_some(),
        };
        if staged {
            self.relist
                .entry(kind_key.to_string())
                .or_default()
                .insert(uid, record);
            return;
        }
        self.store_record(record.clone());
        if should_delete_orphan(&record, self.ready(), &self.exists) {
            delete_object(client, &self.resources, &record).await;
        }
    }

    fn begin_relist(&mut self, kind_key: &str) {
        self.pending_init.insert(kind_key.to_string());
        self.relist.insert(kind_key.to_string(), HashMap::new());
    }

    async fn finish_relist(&mut self, client: &Client, kind_key: &str) {
        let snapshot = self.relist.remove(kind_key).unwrap_or_default();
        let old_uids: Vec<String> = self
            .uid_to_kind
            .iter()
            .filter(|(_, kind)| kind.as_str() == kind_key)
            .map(|(uid, _)| uid.clone())
            .collect();
        for uid in old_uids {
            self.exists.remove(&uid);
            self.uid_to_kind.remove(&uid);
            if let Some(old) = self.objects_with_owners.remove(&uid) {
                for owner in old.owner_uids {
                    if let Some(children) = self.children_of.get_mut(&owner) {
                        children.remove(&uid);
                    }
                }
            }
            self.children_of.remove(&uid);
        }
        self.children_of.retain(|_, children| !children.is_empty());
        let records: Vec<ObjRecord> = snapshot.into_values().collect();
        for record in &records {
            self.store_record(record.clone());
        }
        self.pending_init.remove(kind_key);

        // Only after the complete per-kind snapshot is installed may an
        // orphan decision be made. This also lets owners from another kind
        // that finished relisting in the meantime be considered correctly.
        if self.ready() {
            for record in records {
                if should_delete_orphan(&record, true, &self.exists) {
                    delete_object(client, &self.resources, &record).await;
                }
            }
        }
    }

    async fn handle_delete(&mut self, client: &Client, obj: DynamicObject) {
        let Some(uid) = obj.uid() else { return };
        self.exists.remove(&uid);
        self.uid_to_kind.remove(&uid);
        if let Some(record) = self.objects_with_owners.remove(&uid) {
            for owner in record.owner_uids {
                if let Some(children) = self.children_of.get_mut(&owner) {
                    children.remove(&uid);
                }
            }
        }
        self.children_of.retain(|_, children| !children.is_empty());
        let Some(children) = self.children_of.remove(&uid) else {
            return;
        };
        for child_uid in children {
            let Some(record) = self.objects_with_owners.get(&child_uid).cloned() else {
                continue;
            };
            let any_owner_alive = record.owner_uids.iter().any(|o| self.exists.contains(o));
            if !record.deleting && !any_owner_alive {
                delete_object(client, &self.resources, &record).await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_object_without_owner_references_is_not_an_orphan() {
        assert!(!all_owners_dead(&[], &HashSet::new()));
    }

    #[test]
    fn an_object_with_a_dead_owner_is_an_orphan() {
        assert!(all_owners_dead(&["dead".to_string()], &HashSet::new()));
    }

    #[test]
    fn an_object_with_a_live_owner_is_not_an_orphan() {
        let exists = HashSet::from(["live".to_string()]);
        assert!(!all_owners_dead(&["live".to_string()], &exists));
    }

    #[test]
    fn a_terminating_object_is_not_deleted_again() {
        let record = ObjRecord {
            uid: "child-uid".to_string(),
            gvk_key: "v1/Pod".to_string(),
            namespace: "default".to_string(),
            name: "child".to_string(),
            owner_uids: vec!["dead-owner".to_string()],
            deleting: true,
        };
        assert!(!should_delete_orphan(&record, true, &HashSet::new()));
    }
}

enum GenerationExit {
    CrdChanged,
    StreamsEnded,
}

async fn run_generation(
    client: &Client,
    discovery: Discovery,
    crd_stream: &mut BoxStream<'static, watcher::Result<Event<CustomResourceDefinition>>>,
    crds: &mut HashMap<String, CustomResourceDefinition>,
) -> Result<GenerationExit> {
    let mut resources = HashMap::new();
    let mut streams: Vec<BoxStream<'static, (String, watcher::Result<Event<DynamicObject>>)>> =
        Vec::new();
    for group in discovery.groups() {
        for (ar, caps) in group.recommended_resources() {
            if !should_watch(&ar, &caps) {
                continue;
            }
            let key = gvk_key(&ar);
            resources.insert(key.clone(), ar.clone());
            let key_for_stream = key.clone();
            // Known built-in kinds already have a shared typed watch feeding
            // their ordinary controller(s). Reuse that watch here rather
            // than opening a second dynamic watch for the garbage collector;
            // the small k3s apiserver otherwise spends its watch/concurrency
            // budget on duplicate Pod/PVC/Deployment/etc. streams and can
            // reject CSI's own initial watches with Retry-After.
            let stream = if let Some(stream) =
                crate::watch::watch_dynamic_resource(client, &ar.api_version, &ar.kind)
            {
                stream
            } else {
                // Metadata-only: this controller never reads anything but
                // `.metadata` off these events (see `from_partial_metadata`'s
                // own comment for why, and issue #40 for the profiling data
                // that found the full-body path expensive).
                let api: Api<PartialObjectMeta<DynamicObject>> =
                    Api::all_with((*client).clone(), &ar);
                // Discovery can yield dozens of resource kinds. Admit one
                // ordinary LIST+WATCH at a time below; keeping the initial
                // LIST short avoids holding a long-running watch-list request
                // while CSI sidecars are trying to establish their own.
                watcher(api, watcher::Config::default())
                    .map(|ev| ev.map(map_partial_metadata_event))
                    .boxed()
            };
            let stream = stream
                .map(move |ev| (key_for_stream.clone(), ev))
                .boxed();
            streams.push(stream);
        }
    }

    if resources.is_empty() {
        tracing::warn!("garbage-collector-controller found no watchable/deletable namespaced resource kinds via discovery — nothing to do");
        return Ok(GenerationExit::StreamsEnded);
    }
    tracing::info!(
        kind_count = resources.len(),
        "garbage-collector-controller discovered resource kinds to watch"
    );

    let mut state = State {
        pending_init: resources.keys().cloned().collect(),
        resources,
        exists: HashSet::new(),
        objects_with_owners: HashMap::new(),
        children_of: HashMap::new(),
        uid_to_kind: HashMap::new(),
        relist: HashMap::new(),
    };

    // Discovery commonly returns dozens of kinds. Starting every dynamic
    // watcher at once recreates the same apiserver burst that controller
    // startup admission uses for the typed controllers. Admit one stream at
    // a time; a watch remains in `combined` after its initial list, so this
    // only limits startup fan-out and does not serialize steady-state event
    // handling. GC convergence is deliberately allowed to take seconds at
    // startup rather than competing with CSI, nodelet, and user requests.
    let mut pending_streams = streams.into_iter();
    let mut combined = select_all(Vec::new());
    if let Some(stream) = pending_streams.next() {
        combined.push(stream);
    }
    let admission_period = std::time::Duration::from_secs(1);
    let mut admit = tokio::time::interval_at(
        tokio::time::Instant::now() + admission_period,
        admission_period,
    );

    loop {
        tokio::select! {
            crd_event = crd_stream.next() => {
                match crd_event {
                    Some(Ok(Event::Init)) => crds.clear(),
                    Some(Ok(Event::InitApply(crd))) => {
                        crds.insert(crd.name_any(), crd);
                    }
                    Some(Ok(Event::Apply(crd))) => {
                        let name = crd.name_any();
                        let changed = crds
                            .get(&name)
                            .is_none_or(|previous| previous.spec != crd.spec);
                        crds.insert(name, crd);
                        if changed {
                            tracing::info!(
                                "garbage-collector-controller refreshing resource watches after a CRD change"
                            );
                            return Ok(GenerationExit::CrdChanged);
                        }
                    }
                    Some(Ok(Event::Delete(crd))) => {
                        if crds.remove(&crd.name_any()).is_some() {
                            tracing::info!(
                                "garbage-collector-controller refreshing resource watches after a CRD removal"
                            );
                            return Ok(GenerationExit::CrdChanged);
                        }
                    }
                    Some(Ok(Event::InitDone)) => {}
                    Some(Err(error)) => {
                        tracing::warn!(error = ?error, "CRD watch error in garbage-collector-controller")
                    }
                    None => return Ok(GenerationExit::StreamsEnded),
                }
            }
            event = combined.next() => {
                let Some((kind_key, ev)) = event else {
                    if let Some(stream) = pending_streams.next() {
                        combined.push(stream);
                        continue;
                    }
                    break;
                };
                match ev {
                    Ok(Event::Apply(obj)) => {
                        let staged = state.pending_init.contains(&kind_key);
                        state.handle_apply(client, &kind_key, obj, staged).await;
                    }
                    Ok(Event::InitApply(obj)) => {
                        state.handle_apply(client, &kind_key, obj, true).await;
                    }
                    Ok(Event::Delete(obj)) => {
                        state.handle_delete(client, obj).await;
                    }
                    Ok(Event::Init) => state.begin_relist(&kind_key),
                    Ok(Event::InitDone) => {
                        state.finish_relist(client, &kind_key).await;
                    }
                    Err(e) => {
                        tracing::warn!(kind = %kind_key, error = ?e, "watch error in garbage-collector-controller")
                    }
                }
            }
            _ = admit.tick(), if pending_streams.len() > 0 => {
                if let Some(stream) = pending_streams.next() {
                    combined.push(stream);
                }
            }
        }
    }
    Ok(GenerationExit::StreamsEnded)
}

pub async fn run(client: Client, _cfg: &crate::config::Config) -> Result<()> {
    let mut crd_stream = crate::watch::watch_custom_resource_definitions(&client);
    let mut crds = HashMap::new();

    loop {
        let discovery = crate::watch::discover_api(&client, "garbage-collector-controller").await;
        match run_generation(&client, discovery, &mut crd_stream, &mut crds).await? {
            GenerationExit::CrdChanged => {}
            GenerationExit::StreamsEnded => return Ok(()),
        }
    }
}
