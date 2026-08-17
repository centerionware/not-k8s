//! persistentvolume-binder-controller (Group G, Tier 2 in the plan but
//! load-bearing in practice for this project's own CSI e2e coverage —
//! `csi_pvc.sh`/`csi_attach.sh` gate on `status.phase == Bound`, which
//! nothing sets without this controller once k3s's own copy is disabled):
//! binds a `PersistentVolumeClaim` to a `PersistentVolume`, in both
//! directions this project actually exercises.
//!
//! # Two binding paths
//!
//! **Provisioner-prebound** (the common case for this project's CSI e2e
//! tests): the external-provisioner sidecar creates a new PV with
//! `spec.claimRef` already pointing at the PVC that triggered provisioning.
//! This controller's job here is just to finish the handshake: set
//! `pvc.spec.volumeName` and both objects' `status.phase` to `Bound`.
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
//! # Open verification gap — read before trusting the provisioner-prebound path
//!
//! **The provisioner-prebound path is unverified against real CSI e2e
//! infrastructure as of this writing** — `docs/CONTROLLER_MANAGER.md`'s
//! Group G section has the full diagnostic account. Short version: traced
//! `csi_pvc.sh`/`csi_attach.sh` still skipping under
//! `controller_manager=nodecontroller` past one real bug already fixed here
//! (a missing `root-ca-cert-publisher-controller`) to the reference
//! `external-provisioner` sidecar itself going silent after informer setup
//! — never observed reacting to a real PVC, coincident with the same watch
//! instability (`peer closed connection without sending TLS close_notify`)
//! independently visible in this crate's own controllers' logs. The logic
//! above (`claimed_by`, matching a PV's `spec.claimRef` to the PVC by
//! namespace+name) is exactly what upstream's own binder does and is
//! covered by this file's unit tests, but has never been proven end to end
//! against a real external-provisioner under this crate's own
//! controller-manager. The static-binding path is unaffected and is
//! e2e-verified (`storage_lifecycle_controllers.sh`).
//!
//! **First-match wins for static binding**, not upstream's "smallest PV
//! that still satisfies the request" preference — with no capacity
//! comparison available (see above) there's no meaningful ordering to rank
//! candidates by anyway.

use anyhow::Result;
use futures::StreamExt;
use k8s_openapi::api::core::v1::{
    ObjectReference, PersistentVolume, PersistentVolumeClaim, PersistentVolumeStatus,
};
use kube::api::{Api, Patch, PatchParams};
use kube::runtime::watcher::Event;
use kube::{Client, ResourceExt};
use std::collections::HashMap;

fn is_bound(pvc: &PersistentVolumeClaim) -> bool {
    pvc.spec
        .as_ref()
        .and_then(|s| s.volume_name.as_ref())
        .is_some()
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

async fn reconcile_claim(
    client: &Client,
    pvc: &PersistentVolumeClaim,
    pvs: &mut HashMap<String, PersistentVolume>,
) {
    let namespace = pvc.namespace().unwrap_or_default();
    let name = pvc.name_any();
    let pvc_api: Api<PersistentVolumeClaim> = Api::namespaced(client.clone(), &namespace);
    let pv_api: Api<PersistentVolume> = Api::all(client.clone());

    // The shared PVC informer already delivered the current object. Do not
    // turn every watch event into a second GET; that was both redundant and
    // a major source of request bursts during initial synchronization.
    if is_bound(pvc) {
        return;
    }
    let Some(pv) = pv_for_claim(pvc, pvs).cloned() else {
        return;
    };
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
            "metadata": { "resourceVersion": pv.metadata.resource_version.clone() },
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
                tracing::warn!(pv = %pv_name, namespace = %namespace, pvc = %name, error = ?e, "failed to claim PersistentVolume for static binding");
                return;
            }
        }
    }

    let pvc_patch = serde_json::json!({ "spec": { "volumeName": pv_name } });
    if let Err(e) = pvc_api
        .patch(&name, &PatchParams::default(), &Patch::Merge(&pvc_patch))
        .await
    {
        tracing::warn!(namespace = %namespace, pvc = %name, pv = %pv_name, error = ?e, "failed to set PersistentVolumeClaim.spec.volumeName");
        return;
    }
    let status_patch = serde_json::json!({ "status": { "phase": "Bound" } });
    if let Err(e) = pvc_api
        .patch_status(&name, &PatchParams::default(), &Patch::Merge(&status_patch))
        .await
    {
        tracing::warn!(namespace = %namespace, pvc = %name, error = ?e, "failed to set PersistentVolumeClaim status to Bound");
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
        tracing::warn!(pv = %pv_name, error = ?e, "failed to set PersistentVolume status to Bound");
    }
    tracing::info!(namespace = %namespace, pvc = %name, pv = %pv_name, "persistentvolume-binder-controller bound a PersistentVolumeClaim");
}

fn ns_of<K: ResourceExt>(obj: &K) -> String {
    obj.namespace().unwrap_or_default()
}

pub async fn run(client: Client, _cfg: &crate::config::Config) -> Result<()> {
    let mut pvs: HashMap<String, PersistentVolume> = HashMap::new();
    let mut claims: HashMap<(String, String), PersistentVolumeClaim> = HashMap::new();

    let mut pv_stream = crate::watch::watch_persistent_volumes(&client);
    let mut pvc_stream = crate::watch::watch_persistent_volume_claims(&client);

    loop {
        tokio::select! {
            ev = pv_stream.next() => {
                match ev {
                    Some(Ok(Event::Apply(pv))) | Some(Ok(Event::InitApply(pv))) => {
                        pvs.insert(pv.name_any(), pv);
                        for pvc in claims.values() {
                            reconcile_claim(&client, pvc, &mut pvs).await;
                        }
                    }
                    Some(Ok(Event::Delete(pv))) => { pvs.remove(&pv.name_any()); }
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
                        claims.insert((ns, name), pvc.clone());
                        reconcile_claim(&client, &pvc, &mut pvs).await;
                    }
                    Some(Ok(Event::Delete(pvc))) => { claims.remove(&(ns_of(&pvc), pvc.name_any())); }
                    Some(Ok(Event::Init | Event::InitDone)) => {}
                    Some(Err(e)) => tracing::warn!(error = ?e, "pvc watch error in persistentvolume-binder-controller"),
                    None => return Ok(()),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use k8s_openapi::api::core::v1::{PersistentVolumeClaimSpec, PersistentVolumeSpec};
    use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;

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
