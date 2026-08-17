//! garbage-collector-controller (Group D): owner-reference cascade
//! deletion, generic across every namespaced, watchable, deletable
//! resource kind the apiserver serves — discovered at startup, not a
//! hardcoded list of the kinds this crate happens to already know about.
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
//! **Discovery runs once at startup, not periodically.** A CRD installed
//! after nodecontroller starts is invisible to this controller until it's
//! restarted — upstream re-discovers on a live, invalidatable RESTMapper;
//! that's real complexity this slice doesn't take on. Named, not silently
//! dropped.
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

use anyhow::{Context, Result};
use futures::stream::{select_all, BoxStream, StreamExt};
use kube::api::{Api, DeleteParams, DynamicObject, Preconditions, PropagationPolicy};
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

#[derive(Debug, Clone)]
struct ObjRecord {
    uid: String,
    gvk_key: String,
    namespace: String,
    name: String,
    owner_uids: Vec<String>,
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
        };
        if staged {
            self.relist
                .entry(kind_key.to_string())
                .or_default()
                .insert(uid, record);
            return;
        }
        self.store_record(record.clone());
        let all_dead = self.ready() && all_owners_dead(&owner_uids, &self.exists);
        if all_dead {
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
                if all_owners_dead(&record.owner_uids, &self.exists) {
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
            if !any_owner_alive {
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
}

pub async fn run(client: Client, _cfg: &crate::config::Config) -> Result<()> {
    let discovery = Discovery::new(client.clone())
        .run()
        .await
        .context("running API discovery for garbage-collector-controller")?;

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
                crate::watch::watch_dynamic_resource(&client, &ar.api_version, &ar.kind)
            {
                stream
            } else {
                let api: Api<DynamicObject> = Api::all_with(client.clone(), &ar);
                // Discovery can yield dozens of resource kinds. Use one
                // streaming-list request per kind rather than a synchronized
                // LIST+WATCH burst that competes with ordinary apiserver
                // clients (notably CSI sidecars) during startup.
                watcher(api, watcher::Config::default().streaming_lists()).boxed()
            };
            let stream = stream
                .map(move |ev| (key_for_stream.clone(), ev))
                .boxed();
            streams.push(stream);
        }
    }

    if resources.is_empty() {
        tracing::warn!("garbage-collector-controller found no watchable/deletable namespaced resource kinds via discovery — nothing to do");
        return Ok(());
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
    // startup pacing avoids for the typed controllers. Keep a small active
    // set and admit one more stream periodically; a watch remains in
    // `combined` after its initial list, so this only paces admission and
    // does not serialize steady-state event handling.
    let mut pending_streams = streams.into_iter();
    let mut combined = select_all(Vec::new());
    for _ in 0..4 {
        if let Some(stream) = pending_streams.next() {
            combined.push(stream);
        }
    }
    let mut admit = tokio::time::interval(std::time::Duration::from_millis(250));

    loop {
        tokio::select! {
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
                        state.handle_apply(&client, &kind_key, obj, staged).await;
                    }
                    Ok(Event::InitApply(obj)) => {
                        state.handle_apply(&client, &kind_key, obj, true).await;
                    }
                    Ok(Event::Delete(obj)) => {
                        state.handle_delete(&client, obj).await;
                    }
                    Ok(Event::Init) => state.begin_relist(&kind_key),
                    Ok(Event::InitDone) => {
                        state.finish_relist(&client, &kind_key).await;
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
    Ok(())
}
