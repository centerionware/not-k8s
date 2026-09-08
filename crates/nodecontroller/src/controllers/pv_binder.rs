//! persistentvolume-binder-controller (Group G, Tier 2 in the plan but
//! load-bearing in practice for this project's own CSI e2e coverage —
//! `csi_pvc.sh`/`csi_attach.sh` gate on `status.phase == Bound`, which
//! nothing sets without this controller once k3s's own copy is disabled):
//! binds a `PersistentVolumeClaim` to a `PersistentVolume`, in both
//! directions this project actually exercises.
//!
//! # Two binding paths
//!
//! **Dynamic provisioning** (the common case for this project's CSI e2e
//! tests) is a two-stage handshake. After static matching finds no PV, this
//! controller resolves the PVC's StorageClass and writes
//! `volume.kubernetes.io/storage-provisioner=<class.provisioner>`. The
//! external-provisioner deliberately ignores an unannotated PVC; this write
//! is what asks it to call CSI `CreateVolume`. It then creates a new PV with
//! `spec.claimRef` already pointing at the PVC, and this controller finishes
//! the handshake by setting `pvc.spec.volumeName`, both objects' phase to
//! `Bound`, and finally the `pv.kubernetes.io/bind-completed` publication
//! barrier required by kube-scheduler's VolumeBinding plugin. For
//! `WaitForFirstConsumer`, the provisioner annotation is held
//! until the scheduler has written `volume.kubernetes.io/selected-node`,
//! matching upstream's ordering.
//!
//! **Static** (a PV created by hand, no provisioner involved): finds an
//! unclaimed PV whose `storageClassName` matches (both empty counts as a
//! match) and whose `accessModes` is a superset of the PVC's request, sets
//! `pv.spec.claimRef` to point at the PVC, then proceeds exactly as the
//! prebound path above.
//!
//! # Scope of this slice
//!
//! **No capacity comparison.** `k8s_openapi::Quantity` is a bare string
//! newtype with no arithmetic at all (the same gap `resourcequota-controller`
//! already documents) — static matching considers `storageClassName` and
//! `accessModes` only, not whether the PV is actually large enough. A real
//! difference from upstream, and one that only matters for the static
//! (hand-created PV) path; dynamic provisioning is unaffected since the
//! provisioner itself already created a PV sized for the request.
//!
//! **No unbinding/reclaim.** Once bound, this controller never reconsiders
//! the pairing — release-on-PVC-delete and the `Retain`/`Delete`/`Recycle`
//! reclaim policies are not implemented (matches `stateful_set.rs`'s own
//! documented PVC-lifecycle gap: this crate's PVCs are created with no
//! reclaim automation anywhere yet).
//!
//! **First-match wins for static binding**, not upstream's "smallest PV
//! that still satisfies the request" preference — with no capacity
//! comparison available (see above) there's no meaningful ordering to rank
//! candidates by anyway.

use crate::workqueue::KeyedWorkQueue;
use anyhow::Result;
use futures::StreamExt;
use k8s_openapi::api::core::v1::{
    ObjectReference, PersistentVolume, PersistentVolumeClaim, PersistentVolumeClaimStatus,
    PersistentVolumeStatus,
};
use k8s_openapi::api::storage::v1::StorageClass;
use kube::api::{Api, ListParams, Patch, PatchParams};
use kube::runtime::watcher::Event;
use kube::{Client, ResourceExt};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

const STORAGE_PROVISIONER_ANNOTATION: &str = "volume.kubernetes.io/storage-provisioner";
const SELECTED_NODE_ANNOTATION: &str = "volume.kubernetes.io/selected-node";
const BIND_COMPLETED_ANNOTATION: &str = "pv.kubernetes.io/bind-completed";
const BOUND_BY_CONTROLLER_ANNOTATION: &str = "pv.kubernetes.io/bound-by-controller";
/// A PV/PVC write must never stop this controller from consuming the other
/// informer streams.  In particular, a disconnected apiserver can leave one
/// kube client future pending long enough for newly-created claims to miss the
/// external provisioner's normal hand-off window.
const API_WRITE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
const RECONCILE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);
/// Claims are independent work items. A single FIFO worker lets one slow
/// storage-class lookup or apiserver write hold every later claim behind it;
/// keep ordering per key through `KeyedWorkQueue`, but let unrelated claims
/// make progress concurrently. Sixteen matches the scheduler's normal
/// admission width while keeping this controller bounded on small nodes.
const BINDER_WORKERS: usize = 16;

#[derive(Default)]
struct BinderState {
    pvs: HashMap<String, PersistentVolume>,
    claims: HashMap<(String, String), PersistentVolumeClaim>,
    storage_classes: HashMap<String, StorageClass>,
    /// Safety relists are only needed while the PVC watch is recovering.
    /// Healthy informer delivery already provides the event edge and should
    /// not generate a second periodic LIST against the apiserver.
    pvc_watch_healthy: bool,
    /// A key can be re-enqueued while its API writes are in flight. Keep that
    /// follow-up queued, but never reconcile the same claim concurrently.
    in_flight: HashSet<(String, String)>,
}

fn is_fully_bound(pvc: &PersistentVolumeClaim) -> bool {
    let has_volume = pvc
        .spec
        .as_ref()
        .and_then(|s| s.volume_name.as_deref())
        .is_some_and(|name| !name.is_empty());
    let bind_completed = pvc
        .metadata
        .annotations
        .as_ref()
        .is_some_and(|annotations| annotations.contains_key(BIND_COMPLETED_ANNOTATION));
    has_volume && bind_completed
}

fn access_modes_satisfy(pv: &PersistentVolume, pvc: &PersistentVolumeClaim) -> bool {
    let pv_modes = pv
        .spec
        .as_ref()
        .and_then(|s| s.access_modes.clone())
        .unwrap_or_default();
    let requested = pvc
        .spec
        .as_ref()
        .and_then(|s| s.access_modes.clone())
        .unwrap_or_default();
    requested.iter().all(|m| pv_modes.contains(m))
}

fn storage_class_matches(pv: &PersistentVolume, pvc: &PersistentVolumeClaim) -> bool {
    let pv_class = pv
        .spec
        .as_ref()
        .and_then(|s| s.storage_class_name.clone())
        .unwrap_or_default();
    let pvc_class = pvc
        .spec
        .as_ref()
        .and_then(|s| s.storage_class_name.clone())
        .unwrap_or_default();
    pv_class == pvc_class
}

/// Is `pv` already claimed by exactly `namespace/name`? (Either a
/// provisioner-prebound PV, or one this controller itself just claimed for
/// the static path.)
fn claimed_by(pv: &PersistentVolume, namespace: &str, name: &str) -> bool {
    pv.spec
        .as_ref()
        .and_then(|s| s.claim_ref.as_ref())
        .is_some_and(|r| {
            r.namespace.as_deref() == Some(namespace) && r.name.as_deref() == Some(name)
        })
}

fn is_unclaimed(pv: &PersistentVolume) -> bool {
    pv.spec
        .as_ref()
        .and_then(|s| s.claim_ref.as_ref())
        .is_none()
}

/// The apiserver leaves a newly-created, unclaimed PV in Pending until the
/// PV controller evaluates it. This controller is that replacement, so it
/// must publish Available even when binding is deliberately deferred for a
/// WaitForFirstConsumer static volume. Otherwise the scheduler's static-PV
/// matcher correctly rejects the still-Pending object and no component ever
/// gets to make the node choice.
fn needs_available_phase(pv: &PersistentVolume) -> bool {
    is_unclaimed(pv)
        && matches!(
            pv.status
                .as_ref()
                .and_then(|status| status.phase.as_deref()),
            None | Some("Pending")
        )
}

async fn publish_available_if_needed(client: &Client, pv: &mut PersistentVolume) {
    if !needs_available_phase(pv) {
        return;
    }
    let name = pv.name_any();
    let api: Api<PersistentVolume> = Api::all(client.clone());
    let patch = serde_json::json!({"status": {"phase": "Available"}});
    match api
        .patch_status(&name, &PatchParams::default(), &Patch::Merge(&patch))
        .await
    {
        Ok(updated) => *pv = updated,
        Err(e) => {
            tracing::warn!(pv = %name, error = ?e, "failed to publish Available phase for PersistentVolume")
        }
    }
}

/// Which PV (by name) `pvc` should bind to, if any — pure decision: prefer
/// a PV already claim-ref'd to this exact PVC (the provisioner path),
/// otherwise the first unclaimed PV whose class and access modes satisfy
/// it (the static path).
pub fn pv_for_claim<'a>(
    pvc: &PersistentVolumeClaim,
    pvs: &'a HashMap<String, PersistentVolume>,
) -> Option<&'a PersistentVolume> {
    let namespace = pvc.namespace().unwrap_or_default();
    let name = pvc.name_any();
    if let Some(prebound) = pvs.values().find(|pv| claimed_by(pv, &namespace, &name)) {
        return Some(prebound);
    }
    pvs.values().find(|pv| {
        is_unclaimed(pv) && storage_class_matches(pv, pvc) && access_modes_satisfy(pv, pvc)
    })
}

/// Resolve the external provisioner that should be asked to handle this
/// claim after static matching failed. Upstream's PV controller owns this
/// handoff; the external provisioner does not infer ownership from
/// `storageClassName` alone.
fn provisioner_for_claim<'a>(
    pvc: &PersistentVolumeClaim,
    storage_classes: &'a HashMap<String, StorageClass>,
) -> Option<&'a str> {
    let class_name = pvc.spec.as_ref()?.storage_class_name.as_deref()?;
    let class = storage_classes.get(class_name)?;
    if class.volume_binding_mode.as_deref() == Some("WaitForFirstConsumer") {
        let selected = pvc
            .metadata
            .annotations
            .as_ref()
            .and_then(|annotations| annotations.get(SELECTED_NODE_ANNOTATION));
        if !selected.is_some_and(|node| !node.is_empty()) {
            return None;
        }
    }
    (!class.provisioner.is_empty()).then_some(class.provisioner.as_str())
}

/// A static PV under a WaitForFirstConsumer StorageClass must remain
/// available to the scheduler until a scheduling cycle has made the choice.
/// The scheduler's static PreBind path expresses that choice by writing the
/// PV's claimRef; that pre-bound form is allowed through even though the PVC
/// does not carry the selected-node annotation used by dynamic provisioning.
fn defer_unclaimed_wait_for_first_consumer_pv(
    pvc: &PersistentVolumeClaim,
    pv: &PersistentVolume,
    storage_classes: &HashMap<String, StorageClass>,
) -> bool {
    if !is_unclaimed(pv) {
        return false;
    }
    let Some(class_name) = pvc
        .spec
        .as_ref()
        .and_then(|spec| spec.storage_class_name.as_deref())
    else {
        return false;
    };
    // The PV, PVC, and StorageClass informers are independent streams.  A
    // newly-created PVC can therefore reach this controller before the
    // StorageClass event that explains its binding mode.  Treat a named but
    // not-yet-cached class as unknown, not as Immediate: claiming a matching
    // static PV here would violate WaitForFirstConsumer before the scheduler
    // has had a chance to choose a node.  An explicitly empty class name still
    // means "no class" and is allowed to bind immediately.
    let Some(class) = storage_classes.get(class_name) else {
        return !class_name.is_empty();
    };
    class.volume_binding_mode.as_deref() == Some("WaitForFirstConsumer")
        && !pvc
            .metadata
            .annotations
            .as_ref()
            .is_some_and(|annotations| {
                annotations
                    .get(SELECTED_NODE_ANNOTATION)
                    .is_some_and(|node| !node.is_empty())
            })
}

/// Resolve a named StorageClass through the authoritative shared informer
/// identity when this binder has not seen the class event yet. A static PV is
/// allowed to bind to a PVC whose named class does not exist — the class name
/// is still a valid matching key for the static path — but an existing
/// WaitForFirstConsumer class must not be claimed before scheduling chooses a
/// node. The cache alone cannot distinguish those two cases when the PVC and
/// StorageClass streams deliver events in either order.
async fn lookup_uncached_storage_class(
    name: &str,
) -> std::result::Result<Option<StorageClass>, kube::Error> {
    let classes: Api<StorageClass> = Api::all(crate::watch::base_client());
    classes.get_opt(name).await
}

async fn request_dynamic_provisioning(
    pvc_api: &Api<PersistentVolumeClaim>,
    pvc: &PersistentVolumeClaim,
    provisioner: &str,
) -> bool {
    if pvc
        .metadata
        .annotations
        .as_ref()
        .and_then(|annotations| annotations.get(STORAGE_PROVISIONER_ANNOTATION))
        .is_some_and(|current| current == provisioner)
    {
        return false;
    }

    let name = pvc.name_any();
    let namespace = pvc.namespace().unwrap_or_default();
    let patch = serde_json::json!({
        "metadata": {
            "annotations": { (STORAGE_PROVISIONER_ANNOTATION): provisioner }
        }
    });
    match pvc_api
        .patch(&name, &PatchParams::default(), &Patch::Merge(&patch))
        .await
    {
        Ok(_) => {
            tracing::info!(namespace = %namespace, pvc = %name, %provisioner, "persistentvolume-binder-controller requested dynamic provisioning");
            false
        }
        Err(e) => {
            tracing::warn!(namespace = %namespace, pvc = %name, %provisioner, error = ?e, "failed to request dynamic provisioning for PersistentVolumeClaim; will retry");
            true
        }
    }
}

async fn reconcile_claim(
    client: &Client,
    pvc: &PersistentVolumeClaim,
    pvs: &mut HashMap<String, PersistentVolume>,
    cached_storage_classes: &HashMap<String, StorageClass>,
) -> bool {
    let namespace = pvc.namespace().unwrap_or_default();
    let name = pvc.name_any();
    let pvc_api: Api<PersistentVolumeClaim> = Api::namespaced(client.clone(), &namespace);
    let pv_api: Api<PersistentVolume> = Api::all(client.clone());
    let mut storage_classes = cached_storage_classes.clone();
    let mut named_class_missing = false;

    // A PVC event can be delivered before its StorageClass event, and a
    // shared informer can lose a live event while it is relisting. Resolve a
    // missing named class from the apiserver before deciding that there is
    // nothing to do. The periodic PVC resync below covers the case where the
    // PVC event itself was missed.
    if let Some(class_name) = pvc
        .spec
        .as_ref()
        .and_then(|spec| spec.storage_class_name.as_deref())
        .filter(|name| !name.is_empty())
    {
        if !storage_classes.contains_key(class_name) {
            match lookup_uncached_storage_class(class_name).await {
                Ok(Some(class)) => {
                    storage_classes.insert(class_name.to_string(), class);
                }
                Ok(None) => {
                    // A nonexistent StorageClass does not prevent the
                    // static PV path from matching by name.
                    named_class_missing = true;
                }
                Err(e) => {
                    tracing::warn!(
                        namespace = %namespace,
                        pvc = %name,
                        storage_class = %class_name,
                        error = ?e,
                        "failed to resolve StorageClass during PVC reconcile; will retry"
                    );
                    return true;
                }
            }
        }
    }

    // The shared PVC informer already delivered the current object. Do not
    // turn every watch event into a second GET; that was both redundant and
    // a major source of request bursts during initial synchronization.
    if is_fully_bound(pvc) {
        return false;
    }
    let Some(pv) = pv_for_claim(pvc, pvs).cloned() else {
        if let Some(provisioner) = provisioner_for_claim(pvc, &storage_classes) {
            return request_dynamic_provisioning(&pvc_api, pvc, provisioner).await;
        }
        return false;
    };
    if !named_class_missing
        && defer_unclaimed_wait_for_first_consumer_pv(pvc, &pv, &storage_classes)
    {
        return false;
    }
    let pv_name = pv.name_any();

    if is_unclaimed(&pv) {
        // Static path: claim it first, so a second reconcile racing this
        // one (another PVC also matching this PV) sees it as no longer
        // unclaimed rather than double-claiming it.
        let claim_ref = ObjectReference {
            kind: Some("PersistentVolumeClaim".to_string()),
            namespace: Some(namespace.to_string()),
            name: Some(name.to_string()),
            uid: pvc.uid(),
            ..Default::default()
        };
        // Include the resourceVersion read above in the patch. A concurrent
        // claimant then gets a 409 instead of overwriting the first claim.
        let patch = serde_json::json!({
            "metadata": {
                "resourceVersion": pv.metadata.resource_version.clone(),
                "annotations": { (BOUND_BY_CONTROLLER_ANNOTATION): "yes" }
            },
            "spec": { "claimRef": claim_ref }
        });
        match pv_api
            .patch(&pv_name, &PatchParams::default(), &Patch::Merge(&patch))
            .await
        {
            Ok(updated) => {
                // The watch is asynchronous; update our local cache now so
                // another PVC reconciled in this same loop sees the PV as
                // claimed immediately.
                pvs.insert(pv_name.clone(), updated);
            }
            Err(e) => {
                tracing::warn!(pv = %pv_name, namespace = %namespace, pvc = %name, error = ?e, "failed to claim PersistentVolume for static binding; will retry");
                return true;
            }
        }
    }

    // Upstream records whether the controller selected this volume in the
    // same object update that writes spec.volumeName. BindCompleted is
    // deliberately *not* included yet: kube-scheduler treats that annotation
    // as the publication barrier, so it must be the last successful write.
    let controller_selected_volume = pvc
        .spec
        .as_ref()
        .and_then(|spec| spec.volume_name.as_deref())
        .is_none_or(str::is_empty);
    let mut binding_annotations = serde_json::Map::new();
    if controller_selected_volume {
        binding_annotations.insert(
            BOUND_BY_CONTROLLER_ANNOTATION.to_string(),
            serde_json::Value::String("yes".to_string()),
        );
    }
    let pvc_patch = serde_json::json!({
        "metadata": { "annotations": binding_annotations },
        "spec": { "volumeName": pv_name }
    });
    if let Err(e) = pvc_api
        .patch(&name, &PatchParams::default(), &Patch::Merge(&pvc_patch))
        .await
    {
        tracing::warn!(namespace = %namespace, pvc = %name, pv = %pv_name, error = ?e, "failed to set PersistentVolumeClaim.spec.volumeName; will retry");
        return true;
    }
    let pvc_status = PersistentVolumeClaimStatus {
        access_modes: pv.spec.as_ref().and_then(|spec| spec.access_modes.clone()),
        capacity: pv.spec.as_ref().and_then(|spec| spec.capacity.clone()),
        phase: Some("Bound".to_string()),
        ..pvc.status.clone().unwrap_or_default()
    };
    let status_patch = serde_json::json!({ "status": pvc_status });
    if let Err(e) = pvc_api
        .patch_status(&name, &PatchParams::default(), &Patch::Merge(&status_patch))
        .await
    {
        tracing::warn!(namespace = %namespace, pvc = %name, error = ?e, "failed to set PersistentVolumeClaim status to Bound; will retry");
        return true;
    }
    let pv_status = PersistentVolumeStatus {
        phase: Some("Bound".to_string()),
        ..pv.status.clone().unwrap_or_default()
    };
    let pv_status_patch = serde_json::json!({ "status": pv_status });
    if let Err(e) = pv_api
        .patch_status(
            &pv_name,
            &PatchParams::default(),
            &Patch::Merge(&pv_status_patch),
        )
        .await
    {
        tracing::warn!(pv = %pv_name, error = ?e, "failed to set PersistentVolume status to Bound; will retry");
        return true;
    }

    // kube-scheduler deliberately does not treat spec.volumeName or even a
    // Bound phase as proof that the controller-side handshake is complete.
    // Publish AnnBindCompleted only after every preceding binding write has
    // succeeded. `is_fully_bound` keys off the same marker, so a crash before
    // this point causes the next event to resume rather than abandon a
    // partially completed binding.
    let completed_patch = serde_json::json!({
        "metadata": {
            "annotations": { (BIND_COMPLETED_ANNOTATION): "yes" }
        }
    });
    if let Err(e) = pvc_api
        .patch(
            &name,
            &PatchParams::default(),
            &Patch::Merge(&completed_patch),
        )
        .await
    {
        tracing::warn!(namespace = %namespace, pvc = %name, error = ?e, "failed to publish PersistentVolumeClaim bind completion; will retry");
        return true;
    }
    tracing::info!(namespace = %namespace, pvc = %name, pv = %pv_name, "persistentvolume-binder-controller bound a PersistentVolumeClaim");
    false
}

fn ns_of<K: ResourceExt>(obj: &K) -> String {
    obj.namespace().unwrap_or_default()
}

pub async fn run(client: Client, _cfg: &crate::config::Config) -> Result<()> {
    let state = Arc::new(Mutex::new(BinderState::default()));
    let queue: Arc<KeyedWorkQueue<(String, String)>> = Arc::new(KeyedWorkQueue::default());

    // Watches are the fast path, but a watch restart or broadcast lag must
    // not leave a PVC permanently unprocessed. Periodically refresh the PVC
    // cache and enqueue every current claim; reconciliation is keyed and
    // deduplicated, so this is a bounded safety net rather than a second
    // reconcile loop.
    {
        let client = client.clone();
        let state = state.clone();
        let queue = queue.clone();
        tokio::spawn(async move {
            let api: Api<PersistentVolumeClaim> = Api::all(client);
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(15));
            interval.tick().await;
            loop {
                interval.tick().await;
                let watch_healthy = state
                    .lock()
                    .expect("PV binder state mutex poisoned")
                    .pvc_watch_healthy;
                if watch_healthy {
                    continue;
                }
                match tokio::time::timeout(API_WRITE_TIMEOUT, api.list(&ListParams::default()))
                    .await
                {
                    Ok(Ok(list)) => {
                        let claims = list
                            .items
                            .into_iter()
                            .map(|pvc| ((ns_of(&pvc), pvc.name_any()), pvc))
                            .collect::<HashMap<_, _>>();
                        let keys = claims.keys().cloned().collect::<Vec<_>>();
                        {
                            let mut current = state.lock().expect("PV binder state mutex poisoned");
                            current.claims = claims;
                        }
                        for key in keys {
                            queue.enqueue(key);
                        }
                    }
                    Ok(Err(e)) => {
                        tracing::warn!(error = ?e, "PVC safety resync failed");
                    }
                    Err(_) => {
                        tracing::warn!("PVC safety resync timed out");
                    }
                }
            }
        });
    }

    // Keep watch delivery in the foreground while bounded workers perform
    // reconciliation. A single worker lets one slow storage-class lookup or
    // apiserver write delay every later claim long enough for its test (or
    // workload) to time out. Each worker snapshots state before awaiting API
    // calls, so it never holds the state mutex across the network.
    for worker in 0..BINDER_WORKERS {
        let client = client.clone();
        let state = state.clone();
        let queue = queue.clone();
        tokio::spawn(async move {
            loop {
                let key = queue.pop().await;
                let already_in_flight = {
                    let mut state = state.lock().expect("PV binder state mutex poisoned");
                    !state.in_flight.insert(key.clone())
                };
                if already_in_flight {
                    // A newer event for this key is already queued behind the
                    // active reconciliation. Put this pass back so it is
                    // handled after the current one completes.
                    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                    queue.enqueue(key);
                    continue;
                }
                let (pvc, pvs, storage_classes) = {
                    let state = state.lock().expect("PV binder state mutex poisoned");
                    (
                        state.claims.get(&key).cloned(),
                        state.pvs.clone(),
                        state.storage_classes.clone(),
                    )
                };
                let Some(pvc) = pvc else {
                    state
                        .lock()
                        .expect("PV binder state mutex poisoned")
                        .in_flight
                        .remove(&key);
                    continue;
                };
                let mut pvs = pvs;
                let retry = match tokio::time::timeout(
                    RECONCILE_TIMEOUT,
                    reconcile_claim(&client, &pvc, &mut pvs, &storage_classes),
                )
                .await
                {
                    Ok(retry) => retry,
                    Err(_) => {
                        tracing::warn!(
                            worker,
                            namespace = %key.0,
                            pvc = %key.1,
                            timeout_secs = RECONCILE_TIMEOUT.as_secs(),
                            "timed out reconciling PersistentVolumeClaim; will retry"
                        );
                        true
                    }
                };
                if !retry {
                    if let Some(pv) = pvs.values().find(|pv| {
                        pv.spec
                            .as_ref()
                            .and_then(|spec| spec.claim_ref.as_ref())
                            .and_then(|claim| claim.uid.as_ref())
                            == pvc.uid().as_ref()
                    }) {
                        state
                            .lock()
                            .expect("PV binder state mutex poisoned")
                            .pvs
                            .insert(pv.name_any(), pv.clone());
                    }
                }
                if retry {
                    schedule_retry(&queue, key.clone());
                }
                state
                    .lock()
                    .expect("PV binder state mutex poisoned")
                    .in_flight
                    .remove(&key);
            }
        });
    }

    let mut pv_stream = crate::watch::watch_persistent_volumes(&client);
    let mut pvc_stream = crate::watch::watch_persistent_volume_claims(&client);
    let mut storage_class_stream = crate::watch::watch_storage_classes(&client);

    loop {
        tokio::select! {
            ev = pv_stream.next() => {
                match ev {
                    Some(Ok(Event::Apply(pv))) | Some(Ok(Event::InitApply(pv))) => {
                        // Publishing `Available` is advisory.  Do not await
                        // it in the same select loop that must consume PVC
                        // events; a stalled PATCH otherwise starves every
                        // newly-created claim behind one old PV.
                        let pv = pv;
                        if needs_available_phase(&pv) {
                            let publish_client = client.clone();
                            let publish_name = pv.name_any();
                            let mut publish_pv = pv.clone();
                            tokio::spawn(async move {
                                let result = tokio::time::timeout(
                                    API_WRITE_TIMEOUT,
                                    publish_available_if_needed(&publish_client, &mut publish_pv),
                                )
                                .await;
                                if result.is_err() {
                                    tracing::warn!(pv = %publish_name, "timed out publishing Available phase for PersistentVolume");
                                }
                            });
                        }
                        let keys = {
                            let mut state = state.lock().expect("PV binder state mutex poisoned");
                            state.pvs.insert(pv.name_any(), pv);
                            state
                                .claims
                                .values()
                                .map(|pvc| (ns_of(pvc), pvc.name_any()))
                                .collect::<Vec<_>>()
                        };
                        for key in keys {
                            queue.enqueue(key);
                        }
                    }
                    Some(Ok(Event::Delete(pv))) => {
                        state
                            .lock()
                            .expect("PV binder state mutex poisoned")
                            .pvs
                            .remove(&pv.name_any());
                    }
                    Some(Ok(Event::Init | Event::InitDone)) => {}
                    Some(Err(e)) => tracing::warn!(error = ?e, "pv watch error in persistentvolume-binder-controller"),
                    None => return Ok(()),
                }
            }
            ev = pvc_stream.next() => {
                match ev {
                    Some(Ok(Event::Apply(pvc))) | Some(Ok(Event::InitApply(pvc))) => {
                        let ns = ns_of(&pvc);
                        let name = pvc.name_any();
                        state
                            .lock()
                            .expect("PV binder state mutex poisoned")
                            .claims
                            .insert((ns.clone(), name.clone()), pvc);
                        queue.enqueue((ns, name));
                    }
                    Some(Ok(Event::Delete(pvc))) => {
                        state
                            .lock()
                            .expect("PV binder state mutex poisoned")
                            .claims
                            .remove(&(ns_of(&pvc), pvc.name_any()));
                    }
                    Some(Ok(Event::Init)) => {
                        state
                            .lock()
                            .expect("PV binder state mutex poisoned")
                            .pvc_watch_healthy = false;
                    }
                    Some(Ok(Event::InitDone)) => {
                        state
                            .lock()
                            .expect("PV binder state mutex poisoned")
                            .pvc_watch_healthy = true;
                    }
                    Some(Err(e)) => {
                        state
                            .lock()
                            .expect("PV binder state mutex poisoned")
                            .pvc_watch_healthy = false;
                        tracing::warn!(error = ?e, "pvc watch error in persistentvolume-binder-controller");
                    }
                    None => return Ok(()),
                }
            }
            ev = storage_class_stream.next() => {
                match ev {
                    Some(Ok(Event::Apply(class))) | Some(Ok(Event::InitApply(class))) => {
                        let keys = {
                            let mut state = state.lock().expect("PV binder state mutex poisoned");
                            state.storage_classes.insert(class.name_any(), class);
                            state
                                .claims
                                .values()
                                .map(|pvc| (ns_of(pvc), pvc.name_any()))
                                .collect::<Vec<_>>()
                        };
                        for key in keys {
                            queue.enqueue(key);
                        }
                    }
                    Some(Ok(Event::Delete(class))) => {
                        state
                            .lock()
                            .expect("PV binder state mutex poisoned")
                            .storage_classes
                            .remove(&class.name_any());
                    }
                    Some(Ok(Event::Init | Event::InitDone)) => {}
                    Some(Err(e)) => tracing::warn!(error = ?e, "StorageClass watch error in persistentvolume-binder-controller"),
                    None => return Ok(()),
                }
            }
        }
    }
}

/// A PVC binding handshake has several independent writes. If any one is
/// rejected by a transient apiserver/watch burst, no later object mutation is
/// guaranteed to produce another PVC event. Re-enqueue the claim explicitly
/// so it cannot remain Pending forever after the external provisioner has
/// already created its PV.
const RETRY_DELAY: std::time::Duration = std::time::Duration::from_secs(10);

fn schedule_retry(queue: &std::sync::Arc<KeyedWorkQueue<(String, String)>>, key: (String, String)) {
    let queue = queue.clone();
    tokio::spawn(async move {
        tokio::time::sleep(RETRY_DELAY).await;
        queue.enqueue(key);
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use k8s_openapi::api::core::v1::{PersistentVolumeClaimSpec, PersistentVolumeSpec};
    use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
    use std::collections::BTreeMap;

    fn claim_for_class(class_name: &str) -> PersistentVolumeClaim {
        PersistentVolumeClaim {
            metadata: ObjectMeta::default(),
            spec: Some(PersistentVolumeClaimSpec {
                storage_class_name: Some(class_name.to_string()),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    fn storage_class(name: &str, mode: &str) -> StorageClass {
        StorageClass {
            metadata: ObjectMeta {
                name: Some(name.to_string()),
                ..Default::default()
            },
            provisioner: "hostpath.csi.k8s.io".to_string(),
            volume_binding_mode: Some(mode.to_string()),
            ..Default::default()
        }
    }

    #[test]
    fn immediate_class_requests_its_external_provisioner() {
        let claim = claim_for_class("fast");
        let classes = HashMap::from([("fast".to_string(), storage_class("fast", "Immediate"))]);
        assert_eq!(
            provisioner_for_claim(&claim, &classes),
            Some("hostpath.csi.k8s.io")
        );
    }

    #[test]
    fn wait_for_first_consumer_waits_for_scheduler_selected_node() {
        let mut claim = claim_for_class("delayed");
        let classes = HashMap::from([(
            "delayed".to_string(),
            storage_class("delayed", "WaitForFirstConsumer"),
        )]);
        assert_eq!(provisioner_for_claim(&claim, &classes), None);

        claim.metadata.annotations = Some(BTreeMap::from([(
            SELECTED_NODE_ANNOTATION.to_string(),
            "node-a".to_string(),
        )]));
        assert_eq!(
            provisioner_for_claim(&claim, &classes),
            Some("hostpath.csi.k8s.io")
        );
    }

    #[test]
    fn static_wait_for_first_consumer_binding_waits_until_the_scheduler_prebinds() {
        let claim = claim_for_class("delayed");
        let pv = pv("pv-a", "delayed", &["ReadWriteOnce"], None);
        let classes = HashMap::from([(
            "delayed".to_string(),
            storage_class("delayed", "WaitForFirstConsumer"),
        )]);
        assert!(defer_unclaimed_wait_for_first_consumer_pv(
            &claim, &pv, &classes
        ));

        let mut prebound = pv;
        prebound.spec.as_mut().unwrap().claim_ref = Some(ObjectReference {
            namespace: Some("".to_string()),
            name: Some(claim.name_any()),
            ..Default::default()
        });
        assert!(!defer_unclaimed_wait_for_first_consumer_pv(
            &claim, &prebound, &classes
        ));
    }

    #[test]
    fn an_uncached_named_storage_class_must_not_be_treated_as_immediate() {
        let claim = claim_for_class("created-just-before-the-claim");
        let pv = pv(
            "pv-a",
            "created-just-before-the-claim",
            &["ReadWriteOnce"],
            None,
        );
        assert!(defer_unclaimed_wait_for_first_consumer_pv(
            &claim,
            &pv,
            &HashMap::new()
        ));
    }

    #[test]
    fn an_explicitly_empty_storage_class_can_bind_without_a_class_object() {
        let claim = claim_for_class("");
        let pv = pv("pv-a", "", &["ReadWriteOnce"], None);
        assert!(!defer_unclaimed_wait_for_first_consumer_pv(
            &claim,
            &pv,
            &HashMap::new()
        ));
    }

    #[test]
    fn an_unclaimed_pending_pv_needs_the_available_phase_published() {
        let pv = pv("pv-a", "delayed", &["ReadWriteOnce"], None);
        assert!(needs_available_phase(&pv));

        let mut claimed = pv;
        claimed.spec.as_mut().unwrap().claim_ref = Some(ObjectReference {
            namespace: Some("default".to_string()),
            name: Some("claim".to_string()),
            ..Default::default()
        });
        assert!(!needs_available_phase(&claimed));
    }

    #[test]
    fn scheduler_only_sees_a_claim_as_fully_bound_after_the_completion_barrier() {
        let mut claim = claim_for_class("fast");
        claim.spec.as_mut().unwrap().volume_name = Some("pv-a".to_string());
        assert!(!is_fully_bound(&claim));

        claim.metadata.annotations = Some(BTreeMap::from([(
            BIND_COMPLETED_ANNOTATION.to_string(),
            "yes".to_string(),
        )]));
        assert!(is_fully_bound(&claim));
    }

    fn pvc(name: &str, class: &str, modes: &[&str]) -> PersistentVolumeClaim {
        PersistentVolumeClaim {
            metadata: ObjectMeta {
                name: Some(name.to_string()),
                namespace: Some("default".to_string()),
                ..Default::default()
            },
            spec: Some(PersistentVolumeClaimSpec {
                storage_class_name: Some(class.to_string()),
                access_modes: Some(modes.iter().map(|s| s.to_string()).collect()),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    fn pv(
        name: &str,
        class: &str,
        modes: &[&str],
        claim_ref: Option<(&str, &str)>,
    ) -> PersistentVolume {
        PersistentVolume {
            metadata: ObjectMeta {
                name: Some(name.to_string()),
                ..Default::default()
            },
            spec: Some(PersistentVolumeSpec {
                storage_class_name: Some(class.to_string()),
                access_modes: Some(modes.iter().map(|s| s.to_string()).collect()),
                claim_ref: claim_ref.map(|(ns, n)| ObjectReference {
                    namespace: Some(ns.to_string()),
                    name: Some(n.to_string()),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    #[test]
    fn a_prebound_pv_wins_over_any_static_candidate() {
        let claim = pvc("c1", "fast", &["ReadWriteOnce"]);
        let prebound = pv("pv-1", "fast", &["ReadWriteOnce"], Some(("default", "c1")));
        let pvs = HashMap::from([("pv-1".to_string(), prebound)]);
        assert_eq!(pv_for_claim(&claim, &pvs).unwrap().name_any(), "pv-1");
    }

    #[test]
    fn static_binding_matches_class_and_access_modes() {
        let claim = pvc("c1", "slow", &["ReadWriteOnce"]);
        let wrong_class = pv("pv-1", "fast", &["ReadWriteOnce"], None);
        let right = pv("pv-2", "slow", &["ReadWriteOnce"], None);
        let pvs = HashMap::from([
            ("pv-1".to_string(), wrong_class),
            ("pv-2".to_string(), right),
        ]);
        assert_eq!(pv_for_claim(&claim, &pvs).unwrap().name_any(), "pv-2");
    }

    #[test]
    fn a_pv_claimed_by_someone_else_is_never_a_static_candidate() {
        let claim = pvc("c1", "slow", &["ReadWriteOnce"]);
        let claimed_by_other = pv(
            "pv-1",
            "slow",
            &["ReadWriteOnce"],
            Some(("default", "other")),
        );
        let pvs = HashMap::from([("pv-1".to_string(), claimed_by_other)]);
        assert!(pv_for_claim(&claim, &pvs).is_none());
    }

    #[test]
    fn insufficient_access_modes_is_not_a_match() {
        let claim = pvc("c1", "slow", &["ReadWriteMany"]);
        let too_narrow = pv("pv-1", "slow", &["ReadWriteOnce"], None);
        let pvs = HashMap::from([("pv-1".to_string(), too_narrow)]);
        assert!(pv_for_claim(&claim, &pvs).is_none());
    }
}
