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
//! dead owner has long since been swept elsewhere): once the child's owner
//! kinds have completed their initial lists, any
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
use futures::stream::{select_all, BoxStream, FuturesUnordered, StreamExt};
use k8s_openapi::apiextensions_apiserver::pkg::apis::apiextensions::v1::CustomResourceDefinition;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference;
use kube::api::{Api, DeleteParams, DynamicObject, Preconditions, PropagationPolicy};
use kube::core::PartialObjectMeta;
use kube::discovery::{verbs, Discovery, Scope};
use kube::runtime::watcher;
use kube::runtime::watcher::Event;
use kube::{Client, ResourceExt};
use std::collections::{HashMap, HashSet};
use std::time::Duration;
use tokio::time::Instant;

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
    owner_kinds: Vec<String>,
    /// The full owner references (not just UIDs/kinds): enough identity to
    /// re-check an owner with a direct GET before deleting, so a stale
    /// watch graph can never be the sole evidence a live owner is dead.
    owner_refs: Vec<OwnerReference>,
    deleting: bool,
}

fn owner_refs_of(obj: &DynamicObject) -> Vec<OwnerReference> {
    obj.metadata
        .owner_references
        .as_ref()
        .into_iter()
        .flatten()
        .filter(|o| !o.uid.is_empty())
        .cloned()
        .collect()
}

fn owner_uids_of(obj: &DynamicObject) -> Vec<String> {
    owner_refs_of(obj).into_iter().map(|o| o.uid).collect()
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

/// An owner to re-check directly before deleting a child: its name plus the
/// `ApiResource` needed to GET it. Owners are same-namespace by the API's
/// own rule (`OwnerReference` carries no namespace field — see this
/// module's doc comment), so the child's namespace is the owner's.
#[derive(Debug, Clone)]
struct OwnerTarget {
    namespace: String,
    name: String,
    ar: kube::discovery::ApiResource,
}

/// Result of re-checking a child's owners against the apiserver itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OwnerCheck {
    /// At least one owner still exists — the child must not be deleted.
    Alive,
    /// Every owner is confirmed gone — safe to delete.
    AllGone,
    /// The check itself failed (a transient error): never delete on
    /// uncertain evidence; requeue and re-check on the next retry.
    Uncertain,
}

/// Re-checks every owner of the child with a direct GET. The watch graph
/// (`exists`) is rebuilt from watch events and can miss an owner event — a
/// lagging or selectively-missed informer leaves a live owner looking dead
/// (observed live in e2e: a healthy Service's EndpointSlice was deleted as
/// an "orphan" repeatedly because the Service UID never reached `exists`
/// even though the watch connection itself was healthy). A direct GET is
/// the same final authority upstream's real GC consults before deleting.
async fn check_owners_dead(client: &Client, owners: &[OwnerTarget]) -> OwnerCheck {
    for owner in owners {
        let api: Api<DynamicObject> =
            Api::namespaced_with(client.clone(), &owner.namespace, &owner.ar);
        match api.get(&owner.name).await {
            Ok(_) => return OwnerCheck::Alive,
            Err(kube::Error::Api(status)) if status.is_not_found() => {}
            Err(error) => {
                tracing::warn!(
                    api_version = %owner.ar.api_version, kind = %owner.ar.kind,
                    namespace = %owner.namespace, name = %owner.name, error = ?error,
                    "garbage-collector-controller could not confirm an owner is gone; deferring the orphan deletion"
                );
                return OwnerCheck::Uncertain;
            }
        }
    }
    OwnerCheck::AllGone
}

/// Runs the owner re-check and, only when every owner is confirmed gone,
/// deletes the orphan. Returns whether another attempt is needed (the
/// existing delete-retry contract). A live owner cancels the deletion for
/// good — the owner's own eventual Delete event re-enqueues the child
/// through the normal cascade — and an uncertain check requeues for a fresh
/// re-check after backoff rather than deleting on uncertain evidence.
async fn attempt_orphan_delete(
    client: &Client,
    ar: &kube::discovery::ApiResource,
    record: &ObjRecord,
    owners: &[OwnerTarget],
) -> bool {
    match check_owners_dead(client, owners).await {
        OwnerCheck::Alive => {
            tracing::info!(
                kind = %record.gvk_key, namespace = %record.namespace, name = %record.name,
                uid = %record.uid,
                "garbage-collector-controller cancelled orphan deletion: an owner still exists (the watch graph was stale; the owner's own Delete event will re-enqueue this child)"
            );
            return false;
        }
        OwnerCheck::Uncertain => {
            tracing::warn!(
                kind = %record.gvk_key, namespace = %record.namespace, name = %record.name,
                uid = %record.uid,
                "garbage-collector-controller deferred orphan deletion: owners not confirmed gone; will re-check on the next retry"
            );
            return true;
        }
        OwnerCheck::AllGone => {}
    }
    tracing::debug!(
        kind = %record.gvk_key, namespace = %record.namespace, name = %record.name,
        uid = %record.uid, "garbage-collector-controller attempting orphan deletion"
    );
    match tokio::time::timeout(Duration::from_secs(15), delete_object(client, ar, record)).await {
        Ok(retry) => retry,
        Err(_) => {
            tracing::warn!(
                kind = %record.gvk_key, namespace = %record.namespace, name = %record.name,
                uid = %record.uid, "garbage-collector-controller orphan deletion timed out; retry queued"
            );
            true
        }
    }
}

/// Deletes `record`, background-propagated. Silently ignores "already
/// gone" — the routine outcome of two cascade paths reaching the same
/// child (e.g. discovered both directly and via a since-vanished owner).
/// Returns whether the UID needs another attempt after backoff.
async fn delete_object(
    client: &Client,
    ar: &kube::discovery::ApiResource,
    record: &ObjRecord,
) -> bool {
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
            tracing::info!(kind = %record.gvk_key, namespace = %record.namespace, name = %record.name, uid = %record.uid, "garbage-collector-controller deleted an orphaned object");
            false
        }
        Err(kube::Error::Api(ref status)) if status.is_not_found() => false,
        Err(e) => {
            // A 409 can be a storage CAS race, not just a replacement UID.
            // Never delete a replacement, but do retry the original object.
            if matches!(&e, kube::Error::Api(status) if status.code == 409) {
                match api.get(&record.name).await {
                    Ok(current) if current.uid().as_deref() != Some(record.uid.as_str()) => return false,
                    Err(kube::Error::Api(status)) if status.is_not_found() => return false,
                    _ => {}
                }
            }
            let retry = !matches!(&e, kube::Error::Api(status) if matches!(status.code, 400 | 401 | 403 | 405 | 422));
            tracing::warn!(kind = %record.gvk_key, namespace = %record.namespace, name = %record.name, uid = %record.uid, retry, error = ?e, "garbage-collector-controller failed to delete an orphaned object");
            retry
        }
    }
}

struct DeleteRetry {
    due: Instant,
    delay: Duration,
}

struct State {
    resources: HashMap<String, kube::discovery::ApiResource>,
    exists: HashSet<String>,
    objects_with_owners: HashMap<String, ObjRecord>,
    children_of: HashMap<String, HashSet<String>>,
    pending_init: HashSet<String>,
    uid_to_kind: HashMap<String, String>,
    relist: HashMap<String, HashMap<String, ObjRecord>>,
    deletes: HashMap<String, DeleteRetry>,
}

impl State {
    fn enqueue_delete(&mut self, uid: String) {
        self.deletes.entry(uid).or_insert(DeleteRetry {
            due: Instant::now(),
            delay: Duration::from_secs(1),
        });
    }

    fn complete_delete(&mut self, uid: &str, retry: bool) {
        if !retry {
            self.deletes.remove(uid);
        } else if let Some(entry) = self.deletes.get_mut(uid) {
            entry.due = Instant::now() + entry.delay;
            entry.delay = (entry.delay * 2).min(Duration::from_secs(30));
        }
    }

    fn owners_initialized(&self, record: &ObjRecord) -> bool {
        record.owner_kinds.iter().all(|kind| {
            self.resources.contains_key(kind) && !self.pending_init.contains(kind)
        })
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

    fn handle_apply(
        &mut self,
        kind_key: &str,
        obj: DynamicObject,
        staged: bool,
    ) {
        let Some(uid) = obj.uid() else { return };
        let owner_uids = owner_uids_of(&obj);
        let owner_refs = owner_refs_of(&obj);
        let record = ObjRecord {
            uid: uid.clone(),
            gvk_key: kind_key.to_string(),
            namespace: obj.namespace().unwrap_or_default(),
            name: obj.name_any(),
            owner_uids: owner_uids.clone(),
            owner_kinds: owner_refs
                .iter()
                .map(|owner| format!("{}/{}", owner.api_version, owner.kind))
                .collect(),
            owner_refs,
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
        if should_delete_orphan(&record, self.owners_initialized(&record), &self.exists) {
            self.enqueue_delete(record.uid);
        }
    }

    fn begin_relist(&mut self, kind_key: &str) {
        self.pending_init.insert(kind_key.to_string());
        self.relist.insert(kind_key.to_string(), HashMap::new());
    }

    fn finish_relist(&mut self, kind_key: &str) {
        for record in self.install_snapshot(kind_key) {
            self.enqueue_delete(record.uid);
        }
    }

    fn install_snapshot(&mut self, kind_key: &str) -> Vec<ObjRecord> {
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
            // This index belongs to the children, not the owner snapshot.
            // A Deployment relist must retain ReplicaSet -> Deployment edges.
        }
        self.children_of.retain(|_, children| !children.is_empty());
        let records: Vec<ObjRecord> = snapshot.into_values().collect();
        for record in &records {
            self.store_record(record.clone());
        }
        self.pending_init.remove(kind_key);
        tracing::info!(kind = kind_key, pending = ?self.pending_init,
            "garbage-collector-controller installed resource snapshot");

        // Only after the complete per-kind snapshot is installed may an
        // orphan decision be made. This also lets owners from another kind
        // that finished relisting in the meantime be considered correctly.
        // An owner's completed snapshot can unblock children of ANY kind.
        // Unrelated initial lists must not block a known Deployment -> RS
        // chain. Unknown/uninitialized owner kinds remain protected.
        self.objects_with_owners
            .values()
            .filter(|record| should_delete_orphan(record, self.owners_initialized(record), &self.exists))
            .cloned()
            .collect()
    }

    fn handle_delete(&mut self, obj: DynamicObject) {
        let Some(uid) = obj.uid() else { return };
        self.deletes.remove(&uid);
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
            if should_delete_orphan(&record, self.owners_initialized(&record), &self.exists) {
                self.enqueue_delete(record.uid);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering;

    fn empty_state() -> State {
        State {
            resources: HashMap::from([(
                "apps/v1/Deployment".into(),
                kube::discovery::ApiResource::from_gvk(
                    &kube::core::GroupVersionKind::gvk("apps", "v1", "Deployment"),
                ),
            )]),
            exists: HashSet::new(),
            objects_with_owners: HashMap::new(),
            children_of: HashMap::new(),
            pending_init: HashSet::new(),
            uid_to_kind: HashMap::new(),
            relist: HashMap::new(),
            deletes: HashMap::new(),
        }
    }

    fn record(uid: &str, kind: &str, owners: &[&str]) -> ObjRecord {
        ObjRecord {
            uid: uid.into(),
            gvk_key: kind.into(),
            namespace: "default".into(),
            name: uid.into(),
            owner_uids: owners.iter().map(|owner| (*owner).into()).collect(),
            owner_kinds: owners.iter().map(|_| "apps/v1/Deployment".into()).collect(),
            owner_refs: owners
                .iter()
                .map(|owner| OwnerReference {
                    api_version: "apps/v1".into(),
                    kind: "Deployment".into(),
                    name: (*owner).into(),
                    uid: (*owner).into(),
                    controller: None,
                    block_owner_deletion: None,
                })
                .collect(),
            deleting: false,
        }
    }

    fn deployment_ar() -> kube::discovery::ApiResource {
        kube::discovery::ApiResource::from_gvk(&kube::core::GroupVersionKind::gvk(
            "apps", "v1", "Deployment",
        ))
    }

    /// A kube client whose GETs and DELETEs come from `get_status` (a
    /// `200` response body, a `404`, or a transient error) and
    /// `delete_status` (an HTTP status code). GETs are identified by the
    /// path ending in `/deployment` (the owner name the helpers GET);
    /// anything else on the GET path is unexpected. DELETEs return the
    /// configured status.
    fn gc_client(
        get_status: u16,
        get_body: serde_json::Value,
        delete_status: u16,
        deleted: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    ) -> Client {
        let service = tower::service_fn(move |request: http::Request<kube::client::Body>| {
            let deleted = deleted.clone();
            let get_body = get_body.clone();
            async move {
                let (status, body) = if request.method() == http::Method::GET {
                    (
                        get_status,
                        serde_json::to_vec(&get_body).unwrap_or_default(),
                    )
                } else {
                    assert_eq!(request.method(), http::Method::DELETE);
                    deleted.fetch_add(1, Ordering::Relaxed);
                    (
                        delete_status,
                        serde_json::to_vec(&serde_json::json!({
                            "apiVersion": "v1", "kind": "Status", "status": "Success", "code": 200,
                        })).unwrap_or_default(),
                    )
                };
                Ok::<_, std::convert::Infallible>(http::Response::builder()
                    .status(status)
                    .body(kube::client::Body::from(body))
                    .unwrap())
            }
        });
        Client::new(service, "default")
    }

    #[test]
    fn failed_delete_remains_queued_without_another_watch_event() {
        let mut state = empty_state();
        state.store_record(record("replicaset", "apps/v1/ReplicaSet", &["deployment"]));
        let owner = DynamicObject {
            types: None,
            data: serde_json::Value::Null,
            metadata: kube::api::ObjectMeta {
                uid: Some("deployment".into()),
                ..Default::default()
            },
        };
        state.handle_delete(owner);
        assert!(state.deletes.contains_key("replicaset"));
        state.complete_delete("replicaset", true);
        let due = state.deletes["replicaset"].due;
        assert_eq!(state.deletes["replicaset"].delay, Duration::from_secs(2));
        // A noisy child watch must not bypass the failed request's backoff.
        state.enqueue_delete("replicaset".into());
        assert_eq!(state.deletes["replicaset"].due, due);
        for _ in 0..10 {
            state.complete_delete("replicaset", true);
        }
        assert_eq!(state.deletes["replicaset"].delay, Duration::from_secs(30));
        state.complete_delete("replicaset", false);
        assert!(state.deletes.is_empty());
    }

    #[test]
    fn child_delete_cancels_a_pending_retry() {
        let mut state = empty_state();
        state.enqueue_delete("old-uid".into());
        state.handle_delete(DynamicObject {
            types: None,
            data: serde_json::Value::Null,
            metadata: kube::api::ObjectMeta {
                uid: Some("old-uid".into()),
                ..Default::default()
            },
        });
        // Completion of an already-running failed request cannot resurrect it.
        state.complete_delete("old-uid", true);
        assert!(state.deletes.is_empty());
    }

    #[tokio::test]
    async fn delete_conflict_retries_same_uid_but_preserves_replacement() {
        for (live_uid, expected_retry) in [("child-uid", true), ("replacement-uid", false)] {
            let service = tower::service_fn(move |request: http::Request<kube::client::Body>| async move {
                let (status, body) = if request.method() == http::Method::DELETE {
                    (409, serde_json::json!({
                        "apiVersion": "v1", "kind": "Status", "status": "Failure",
                        "reason": "Conflict", "message": "compare failed", "code": 409,
                    }))
                } else {
                    assert_eq!(request.method(), http::Method::GET);
                    (200, serde_json::json!({
                        "apiVersion": "apps/v1", "kind": "ReplicaSet",
                        "metadata": {"name": "child-uid", "uid": live_uid},
                    }))
                };
                Ok::<_, std::convert::Infallible>(http::Response::builder()
                    .status(status)
                    .body(kube::client::Body::from(serde_json::to_vec(&body).unwrap()))
                    .unwrap())
            });
            let client = Client::new(service, "default");
            let ar = kube::discovery::ApiResource::from_gvk(
                &kube::core::GroupVersionKind::gvk("apps", "v1", "ReplicaSet"),
            );
            let child = record("child-uid", "apps/v1/ReplicaSet", &["gone"]);
            assert_eq!(delete_object(&client, &ar, &child).await, expected_retry);
        }
    }

    #[test]
    fn owner_relist_preserves_children_from_other_kinds() {
        let mut state = empty_state();
        let owner = record("deployment", "apps/v1/Deployment", &[]);
        state.store_record(owner.clone());
        state.store_record(record("replicaset", "apps/v1/ReplicaSet", &["deployment"]));
        state.begin_relist(&owner.gvk_key);
        state.relist.get_mut(&owner.gvk_key).unwrap().insert(owner.uid.clone(), owner.clone());
        assert!(state.install_snapshot(&owner.gvk_key).is_empty());
        assert!(state.children_of["deployment"].contains("replicaset"));
    }

    #[test]
    fn final_initial_list_checks_orphans_from_earlier_kinds() {
        let mut state = empty_state();
        state.begin_relist("apps/v1/ReplicaSet");
        state.begin_relist("apps/v1/Deployment");
        state.relist.get_mut("apps/v1/ReplicaSet").unwrap().insert(
            "replicaset".into(), record("replicaset", "apps/v1/ReplicaSet", &["gone"]),
        );
        assert!(state.install_snapshot("apps/v1/ReplicaSet").is_empty());
        let orphans = state.install_snapshot("apps/v1/Deployment");
        assert_eq!(orphans.len(), 1);
        assert_eq!(orphans[0].uid, "replicaset");
    }

    #[test]
    fn owner_missing_from_relist_exposes_existing_child_as_orphan() {
        let mut state = empty_state();
        state.store_record(record("deployment", "apps/v1/Deployment", &[]));
        state.store_record(record("replicaset", "apps/v1/ReplicaSet", &["deployment"]));
        state.begin_relist("apps/v1/Deployment");
        let orphans = state.install_snapshot("apps/v1/Deployment");
        assert_eq!(orphans.len(), 1);
        assert_eq!(orphans[0].uid, "replicaset");
    }

    #[test]
    fn unrelated_initial_list_does_not_block_a_known_orphan() {
        let mut state = empty_state();
        state.begin_relist("resource.k8s.io/v1/ResourceClaim");
        state.begin_relist("apps/v1/Deployment");
        state.store_record(record("replicaset", "apps/v1/ReplicaSet", &["gone"]));
        let orphans = state.install_snapshot("apps/v1/Deployment");
        assert_eq!(orphans.len(), 1);
        assert_eq!(orphans[0].uid, "replicaset");
        assert!(!state.pending_init.is_empty());
    }

    #[test]
    fn unknown_or_uninitialized_owner_kinds_are_not_assumed_dead() {
        let mut state = empty_state();
        let child = record("replicaset", "apps/v1/ReplicaSet", &["owner"]);
        state.begin_relist("apps/v1/Deployment");
        assert!(!state.owners_initialized(&child));
        state.pending_init.clear();
        state.resources.clear();
        assert!(!state.owners_initialized(&child));
    }

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
            owner_kinds: vec!["apps/v1/Deployment".to_string()],
            owner_refs: Vec::new(),
            deleting: true,
        };
        assert!(!should_delete_orphan(&record, true, &HashSet::new()));
    }

    #[tokio::test]
    async fn delete_is_cancelled_when_an_owner_still_exists() {
        // The watch graph claims the owner is dead (`exists` lacks it), but
        // a direct GET finds the owner alive — the live e2e failure (a
        // healthy Service's EndpointSlice deleted as an "orphan"). The
        // deletion must be cancelled, not attempted.
        let deleted = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let owner_body = serde_json::json!({
            "apiVersion": "apps/v1", "kind": "Deployment",
            "metadata": {"name": "deployment", "uid": "deployment"},
        });
        let client = gc_client(200, owner_body, 200, deleted.clone());
        let child = record("child", "apps/v1/ReplicaSet", &["deployment"]);
        let owner = OwnerTarget {
            namespace: "default".into(),
            name: "deployment".into(),
            ar: deployment_ar(),
        };
        let retry = attempt_orphan_delete(&client, &deployment_ar(), &child, &[owner]).await;
        assert!(
            !retry,
            "a live owner must cancel the deletion outright, not requeue it"
        );
        assert_eq!(
            deleted.load(std::sync::atomic::Ordering::Relaxed),
            0,
            "a live owner must never reach the DELETE call"
        );
    }

    #[tokio::test]
    async fn delete_proceeds_when_every_owner_is_confirmed_gone() {
        let deleted = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let not_found = serde_json::json!({
            "apiVersion": "v1", "kind": "Status", "status": "Failure",
            "reason": "NotFound", "message": "deployments.apps \"deployment\" not found", "code": 404,
        });
        let client = gc_client(404, not_found, 200, deleted.clone());
        let child = record("child", "apps/v1/ReplicaSet", &["deployment"]);
        let owner = OwnerTarget {
            namespace: "default".into(),
            name: "deployment".into(),
            ar: deployment_ar(),
        };
        let retry = attempt_orphan_delete(&client, &deployment_ar(), &child, &[owner]).await;
        assert!(!retry, "a confirmed orphan should delete once and not retry");
        assert_eq!(deleted.load(std::sync::atomic::Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn delete_is_deferred_when_the_owner_check_cannot_confirm() {
        // A transient GET error is not evidence the owner is dead: never
        // delete on uncertain evidence — requeue for a fresh re-check.
        let deleted = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let server_error = serde_json::json!({
            "apiVersion": "v1", "kind": "Status", "status": "Failure",
            "message": "temporary", "code": 500,
        });
        let client = gc_client(500, server_error, 200, deleted.clone());
        let child = record("child", "apps/v1/ReplicaSet", &["deployment"]);
        let owner = OwnerTarget {
            namespace: "default".into(),
            name: "deployment".into(),
            ar: deployment_ar(),
        };
        let retry = attempt_orphan_delete(&client, &deployment_ar(), &child, &[owner]).await;
        assert!(retry, "an uncertain owner check must requeue for re-checking");
        assert_eq!(
            deleted.load(std::sync::atomic::Ordering::Relaxed),
            0,
            "an uncertain owner check must never reach the DELETE call"
        );
    }
}

enum GenerationExit {
    CrdChanged,
    StreamsEnded,
}

const CRD_REFRESH_QUIET_PERIOD: std::time::Duration = std::time::Duration::from_millis(250);

fn update_crd_cache(
    crds: &mut HashMap<String, CustomResourceDefinition>,
    event: Event<CustomResourceDefinition>,
) -> bool {
    match event {
        Event::Init => {
            crds.clear();
            false
        }
        Event::InitApply(crd) => {
            crds.insert(crd.name_any(), crd);
            false
        }
        Event::Apply(crd) => {
            let name = crd.name_any();
            let changed = crds
                .get(&name)
                .is_none_or(|previous| previous.spec != crd.spec);
            crds.insert(name, crd);
            changed
        }
        Event::Delete(crd) => crds.remove(&crd.name_any()).is_some(),
        Event::InitDone => false,
    }
}

async fn coalesce_crd_changes(
    crd_stream: &mut BoxStream<'static, watcher::Result<Event<CustomResourceDefinition>>>,
    crds: &mut HashMap<String, CustomResourceDefinition>,
) -> GenerationExit {
    loop {
        match tokio::time::timeout(CRD_REFRESH_QUIET_PERIOD, crd_stream.next()).await {
            Err(_) => return GenerationExit::CrdChanged,
            Ok(Some(Ok(event))) => {
                update_crd_cache(crds, event);
            }
            Ok(Some(Err(error))) => {
                tracing::warn!(error = ?error, "CRD watch error while coalescing garbage-collector refresh")
            }
            Ok(None) => return GenerationExit::StreamsEnded,
        }
    }
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
                // Discovery can yield dozens of resource kinds. Admit one
                // ordinary LIST+WATCH at a time below; keeping the initial
                // LIST short avoids holding a long-running watch-list request
                // while CSI sidecars are trying to establish their own.
                crate::watch::watch_dynamic_metadata_resource(client, &ar)
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
        deletes: HashMap::new(),
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
    // One active request, one queued entry per UID. Keep consuming watches
    // during network I/O; an unavailable API must not freeze the owner graph.
    let mut deleting: FuturesUnordered<futures::future::BoxFuture<'static, (String, bool)>> =
        FuturesUnordered::new();

    loop {
        let next_delete = state.deletes.iter()
            .min_by_key(|(_, entry)| entry.due)
            .map(|(uid, entry)| (uid.clone(), entry.due));
        let delete_due = next_delete.as_ref().map(|(_, due)| *due)
            .unwrap_or_else(|| Instant::now() + Duration::from_secs(30));
        tokio::select! {
            Some((uid, retry)) = deleting.next(), if !deleting.is_empty() => {
                state.complete_delete(&uid, retry);
            }
            _ = tokio::time::sleep_until(delete_due), if deleting.is_empty() && next_delete.is_some() => {
                let Some((uid, _)) = next_delete else { continue };
                let Some(record) = state.objects_with_owners.get(&uid).cloned() else {
                    state.deletes.remove(&uid);
                    continue;
                };
                // Recompute from the latest graph, never retry a stale orphan
                // decision after adoption, deletion, or an incomplete relist.
                if !should_delete_orphan(&record, state.owners_initialized(&record), &state.exists) {
                    state.deletes.remove(&uid);
                    continue;
                }
                let Some(ar) = state.resources.get(&record.gvk_key).cloned() else {
                    state.deletes.remove(&uid);
                    continue;
                };
                // Resolve each owner to a direct GET target before moving
                // into the spawned future. The watch graph can be stale
                // (see `check_owners_dead`), so the future re-checks every
                // owner against the apiserver and only deletes when all
                // are confirmed gone.
                let owner_targets: Vec<OwnerTarget> = record
                    .owner_refs
                    .iter()
                    .filter_map(|owner| {
                        let key = format!("{}/{}", owner.api_version, owner.kind);
                        let owner_ar = state.resources.get(&key).cloned()?;
                        Some(OwnerTarget {
                            namespace: record.namespace.clone(),
                            name: owner.name.clone(),
                            ar: owner_ar,
                        })
                    })
                    .collect();
                let client = client.clone();
                deleting.push(Box::pin(async move {
                    let retry =
                        attempt_orphan_delete(&client, &ar, &record, &owner_targets).await;
                    (uid, retry)
                }));
            }
            crd_event = crd_stream.next() => {
                match crd_event {
                    Some(Ok(event)) => {
                        if update_crd_cache(&mut *crds, event) {
                            tracing::info!(
                                "garbage-collector-controller refreshing resource watches after a CRD change"
                            );
                            return Ok(coalesce_crd_changes(crd_stream, crds).await);
                        }
                    }
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
                        state.handle_apply(&kind_key, obj, staged);
                    }
                    Ok(Event::InitApply(obj)) => {
                        state.handle_apply(&kind_key, obj, true);
                    }
                    Ok(Event::Delete(obj)) => {
                        state.handle_delete(obj);
                    }
                    Ok(Event::Init) => state.begin_relist(&kind_key),
                    Ok(Event::InitDone) => {
                        state.finish_relist(&kind_key);
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
