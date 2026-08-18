//! Projections of the storage API objects Phase 4's plugins need:
//! PersistentVolume, PersistentVolumeClaim, StorageClass, CSINode, CSIDriver,
//! CSIStorageCapacity.
//!
//! # Why these live wholesale in the `Cache`, not incrementally like nodes
//!
//! Nodes and pods are copied into the snapshot incrementally (see
//! `snapshot.rs`'s module header) because a cluster can have thousands of
//! them and a scheduling cycle runs once per pending pod. Storage objects are
//! a different shape: a cluster typically has PVs/PVCs in the hundreds at
//! most, StorageClasses and CSIDrivers in the single digits, and none of them
//! change anywhere near as often as a node's heartbeat. Copying the whole set
//! per snapshot is the same trade the namespace and PDB mirrors already make,
//! for the same reason — the incremental machinery would cost more code than
//! the copy it avoids.

use k8s_openapi::api::core::v1::{
    NodeSelector, PersistentVolume, PersistentVolumeClaim, TopologySelectorTerm,
};
use k8s_openapi::api::storage::v1::{
    CSIDriver, CSINode, CSIStorageCapacity, StorageClass, VolumeAttachment,
};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::LabelSelector;
use std::collections::BTreeMap;

const BETA_STORAGE_CLASS_ANNOTATION: &str = "volume.beta.kubernetes.io/storage-class";
const SELECTED_NODE_ANNOTATION: &str = "volume.kubernetes.io/selected-node";

/// Published by the PV binder only after the PVC/PV binding transaction is
/// complete. kube-scheduler uses this as the completion barrier instead of
/// trusting a status field that can be observed between writes.
pub const BIND_COMPLETED_ANNOTATION: &str = "pv.kubernetes.io/bind-completed";

pub fn pvc_is_fully_bound(pvc: &PersistentVolumeClaim) -> bool {
    pvc.spec
        .as_ref()
        .and_then(|s| s.volume_name.as_deref())
        .is_some_and(|name| !name.is_empty())
        && pvc
            .metadata
            .annotations
            .as_ref()
            .is_some_and(|a| a.contains_key(BIND_COMPLETED_ANNOTATION))
}

/// Bytes, from a `resource.k8s.io` `Quantity` — reuses `pod.rs`'s parser
/// rather than a second implementation of the same suffix table.
fn quantity_bytes(q: &k8s_openapi::apimachinery::pkg::api::resource::Quantity) -> i64 {
    super::pod::parse_quantity(&q.0)
}

/// A PersistentVolume, projected to what the storage plugins read.
#[derive(Clone, Debug, Default)]
pub struct PvInfo {
    pub name: String,
    pub access_modes: Vec<String>,
    pub capacity_bytes: i64,
    /// `spec.claimRef`, if this PV is bound or pre-bound to a claim.
    pub claim_ref: Option<(String, String)>,
    /// Whether `spec.claimRef` existed even if it was malformed and omitted a
    /// namespace or name. Upstream treats any such reference as claimed, not
    /// as a free volume.
    pub claim_ref_present: bool,
    /// `spec.claimRef.uid`, kept separately because a deleted-and-recreated
    /// PVC with the same namespace/name is not the claim this PV was bound
    /// to. An empty UID is the user-prebound form upstream permits.
    pub claim_ref_uid: Option<String>,
    pub storage_class_name: String,
    /// `spec.nodeAffinity.required` — the hard constraint `VolumeBinding`'s
    /// Filter checks a candidate node against.
    pub node_affinity: Option<Box<NodeSelector>>,
    /// The PV's own labels, for the legacy `failure-domain.beta.kubernetes.io/zone`
    /// / `topology.kubernetes.io/zone` convention `VolumeZone` reads when a PV
    /// predates (or simply does not use) `nodeAffinity`.
    pub labels: BTreeMap<String, String>,
    /// `spec.csi.driver`, for `NodeVolumeLimits`. `None` for a PV backed by a
    /// non-CSI (legacy in-tree) source — those never count against a CSI
    /// driver's per-node limit.
    pub csi_driver: Option<String>,
    /// `spec.csi.volumeHandle`, paired with `csi_driver` to form the unique
    /// identity NodeVolumeLimits counts. PVCs are mounts; handles are
    /// attachable volumes, and several PVC/pod references to one handle must
    /// consume one slot rather than several.
    pub csi_volume_handle: Option<String>,
    /// `status.phase`. A static-PV match (`volume_binding.rs`) only
    /// considers a non-pre-bound PV usable when this is `"Available"` — a
    /// `Released`/`Failed`/`Pending` PV binding successfully would either
    /// never actually complete or hand a pod storage the PV controller
    /// considers unfit for reuse.
    pub phase: String,
    /// `spec.volumeMode`, defaulting to `"Filesystem"` when unset — the same
    /// default upstream's API defaulting applies. A static-PV match must
    /// require this to equal the PVC's own requested mode; binding a `Block`
    /// PV to a `Filesystem` claim (or the reverse) fails at mount time, not
    /// at match time, so nothing before `PreBind` would ever catch it.
    pub volume_mode: String,
    /// Kubernetes 1.33 leaves VolumeAttributesClass disabled by default. In
    /// that mode upstream refuses any PV carrying this field rather than
    /// silently binding it without the feature's semantics.
    pub volume_attributes_class_name: Option<String>,
    /// A terminating PV is never a static-binding candidate.
    pub deleting: bool,
}

impl PvInfo {
    pub fn from_api(pv: &PersistentVolume) -> Self {
        let spec = pv.spec.clone().unwrap_or_default();
        let annotations = pv.metadata.annotations.clone().unwrap_or_default();
        let claim_ref_present = spec.claim_ref.is_some();
        PvInfo {
            name: pv.metadata.name.clone().unwrap_or_default(),
            access_modes: spec.access_modes.unwrap_or_default(),
            capacity_bytes: spec
                .capacity
                .as_ref()
                .and_then(|c| c.get("storage"))
                .map(quantity_bytes)
                .unwrap_or(0),
            claim_ref: spec.claim_ref.as_ref().and_then(|r| {
                Some((r.namespace.clone()?, r.name.clone()?))
            }),
            claim_ref_present,
            claim_ref_uid: spec
                .claim_ref
                .as_ref()
                .and_then(|reference| reference.uid.clone())
                .filter(|uid| !uid.is_empty()),
            storage_class_name: annotations
                .get(BETA_STORAGE_CLASS_ANNOTATION)
                .cloned()
                .or(spec.storage_class_name)
                .unwrap_or_default(),
            node_affinity: spec.node_affinity.and_then(|na| na.required).map(Box::new),
            labels: pv.metadata.labels.clone().unwrap_or_default(),
            csi_driver: spec.csi.as_ref().map(|c| c.driver.clone()),
            csi_volume_handle: spec.csi.map(|c| c.volume_handle),
            phase: pv.status.as_ref().and_then(|s| s.phase.clone()).unwrap_or_default(),
            volume_mode: spec.volume_mode.unwrap_or_else(|| "Filesystem".to_string()),
            volume_attributes_class_name: spec.volume_attributes_class_name,
            deleting: pv.metadata.deletion_timestamp.is_some(),
        }
    }
}

/// A PersistentVolumeClaim, projected.
#[derive(Clone, Debug, Default)]
pub struct PvcInfo {
    pub namespace: String,
    pub name: String,
    pub uid: String,
    pub storage_class_name: Option<String>,
    /// `spec.volumeName` — set once bound (or pre-bound by an admin/user
    /// pointing a claim at a specific PV).
    pub volume_name: Option<String>,
    pub requested_access_modes: Vec<String>,
    pub requested_bytes: i64,
    pub selector: Option<LabelSelector>,
    /// Upstream's `isPVCFullyBound`: `spec.volumeName` is non-empty and the
    /// PV binder has published `pv.kubernetes.io/bind-completed`.
    pub bound: bool,
    /// `spec.volumeMode`, defaulting to `"Filesystem"` when unset — see
    /// `PvInfo::volume_mode`'s doc comment for why a static match must
    /// require this to equal the candidate PV's own mode.
    pub volume_mode: String,
    pub volume_attributes_class_name: Option<String>,
    /// Existing delayed-binding decision. On a retry, upstream only permits
    /// this node and continues provisioning instead of rematching static PVs.
    pub selected_node: Option<String>,
    /// `status.phase`, used for the special Lost rejection before any bind
    /// work is attempted.
    pub phase: String,
    pub deleting: bool,
    /// UID of the controller owner reference, if any. Generic ephemeral
    /// volumes may only consume the deterministic PVC created for this Pod.
    pub controller_owner_uid: Option<String>,
}

impl PvcInfo {
    pub fn key(&self) -> String {
        format!("{}/{}", self.namespace, self.name)
    }

    pub fn wants_read_write_once_pod(&self) -> bool {
        self.requested_access_modes.iter().any(|m| m == "ReadWriteOncePod")
    }

    pub fn from_api(pvc: &PersistentVolumeClaim) -> Self {
        let spec = pvc.spec.clone().unwrap_or_default();
        let annotations = pvc.metadata.annotations.clone().unwrap_or_default();
        let requested_bytes = spec
            .resources
            .as_ref()
            .and_then(|r| r.requests.as_ref())
            .and_then(|r| r.get("storage"))
            .map(quantity_bytes)
            .unwrap_or(0);
        let volume_name = spec.volume_name.filter(|name| !name.is_empty());
        let bound = pvc_is_fully_bound(pvc);
        PvcInfo {
            namespace: pvc.metadata.namespace.clone().unwrap_or_default(),
            name: pvc.metadata.name.clone().unwrap_or_default(),
            uid: pvc.metadata.uid.clone().unwrap_or_default(),
            storage_class_name: annotations
                .get(BETA_STORAGE_CLASS_ANNOTATION)
                .cloned()
                .or(spec.storage_class_name),
            volume_name,
            requested_access_modes: spec.access_modes.unwrap_or_default(),
            requested_bytes,
            selector: spec.selector,
            bound,
            volume_mode: spec.volume_mode.unwrap_or_else(|| "Filesystem".to_string()),
            volume_attributes_class_name: spec.volume_attributes_class_name,
            selected_node: annotations.get(SELECTED_NODE_ANNOTATION).cloned(),
            phase: pvc.status.as_ref().and_then(|s| s.phase.clone()).unwrap_or_default(),
            deleting: pvc.metadata.deletion_timestamp.is_some(),
            controller_owner_uid: pvc
                .metadata
                .owner_references
                .as_ref()
                .and_then(|owners| owners.iter().find(|owner| owner.controller == Some(true)))
                .map(|owner| owner.uid.clone()),
        }
    }
}

/// The only VolumeAttachment fields NodeVolumeLimits reads.
#[derive(Clone, Debug, Default)]
pub struct VolumeAttachmentInfo {
    pub node_name: String,
    pub attacher: String,
    pub pv_name: Option<String>,
}

impl VolumeAttachmentInfo {
    pub fn from_api(attachment: &VolumeAttachment) -> Self {
        Self {
            node_name: attachment.spec.node_name.clone(),
            attacher: attachment.spec.attacher.clone(),
            pv_name: attachment.spec.source.persistent_volume_name.clone(),
        }
    }
}

/// A StorageClass, projected.
#[derive(Clone, Debug, Default)]
pub struct StorageClassInfo {
    pub name: String,
    pub provisioner: String,
    /// `volumeBindingMode == "WaitForFirstConsumer"`. Upstream defaults to
    /// `Immediate` when unset, which is `false` here.
    pub wait_for_first_consumer: bool,
    pub allowed_topologies: Vec<TopologySelectorTerm>,
}

impl StorageClassInfo {
    pub fn from_api(sc: &StorageClass) -> Self {
        StorageClassInfo {
            name: sc.metadata.name.clone().unwrap_or_default(),
            provisioner: sc.provisioner.clone(),
            wait_for_first_consumer: sc.volume_binding_mode.as_deref() == Some("WaitForFirstConsumer"),
            allowed_topologies: sc.allowed_topologies.clone().unwrap_or_default(),
        }
    }
}

/// A node's CSINode: which drivers are registered there and each one's
/// volume-count ceiling.
#[derive(Clone, Debug, Default)]
pub struct CsiNodeInfo {
    /// driver name -> `allocatable.count`. `None` means unbounded.
    pub drivers: BTreeMap<String, Option<i32>>,
}

impl CsiNodeInfo {
    pub fn from_api(node: &CSINode) -> Self {
        CsiNodeInfo {
            drivers: node
                .spec
                .drivers
                .iter()
                .map(|d| (d.name.clone(), d.allocatable.as_ref().and_then(|a| a.count)))
                .collect(),
        }
    }
}

/// A CSIDriver, projected. The only field `NodeVolumeLimits`/`VolumeBinding`
/// read off it today.
#[derive(Clone, Debug, Default)]
pub struct CsiDriverInfo {
    pub storage_capacity: bool,
}

impl CsiDriverInfo {
    pub fn from_api(driver: &CSIDriver) -> Self {
        CsiDriverInfo { storage_capacity: driver.spec.storage_capacity.unwrap_or(false) }
    }
}

/// A CSIStorageCapacity, projected.
#[derive(Clone, Debug, Default)]
pub struct StorageCapacityInfo {
    pub storage_class_name: String,
    pub node_topology: Option<LabelSelector>,
    /// `None` means the driver reported no capacity at all for this pool,
    /// which upstream treats as "cannot fit anything" rather than unbounded.
    pub capacity_bytes: Option<i64>,
}

impl StorageCapacityInfo {
    pub fn from_api(c: &CSIStorageCapacity) -> Self {
        StorageCapacityInfo {
            storage_class_name: c.storage_class_name.clone(),
            node_topology: c.node_topology.clone(),
            capacity_bytes: c.capacity.as_ref().map(quantity_bytes),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use k8s_openapi::api::core::v1::{
        ObjectReference, PersistentVolumeClaimSpec, PersistentVolumeClaimStatus,
        PersistentVolumeSpec,
    };
    use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;

    fn pvc(volume_name: Option<&str>, completed: bool, phase: Option<&str>) -> PersistentVolumeClaim {
        let annotations = completed.then(|| {
            BTreeMap::from([(BIND_COMPLETED_ANNOTATION.to_string(), "yes".to_string())])
        });
        PersistentVolumeClaim {
            metadata: ObjectMeta { annotations, ..Default::default() },
            spec: Some(PersistentVolumeClaimSpec {
                volume_name: volume_name.map(str::to_string),
                ..Default::default()
            }),
            status: Some(PersistentVolumeClaimStatus {
                phase: phase.map(str::to_string),
                ..Default::default()
            }),
        }
    }

    #[test]
    fn phase_bound_without_the_completion_annotation_is_not_fully_bound() {
        assert!(!pvc_is_fully_bound(&pvc(Some("pv"), false, Some("Bound"))));
    }

    #[test]
    fn volume_name_and_completion_annotation_are_the_publication_barrier() {
        assert!(pvc_is_fully_bound(&pvc(Some("pv"), true, None)));
        assert!(!pvc_is_fully_bound(&pvc(None, true, Some("Bound"))));
    }

    #[test]
    fn storage_projection_keeps_claim_identity_and_legacy_class_precedence() {
        let annotations = Some(BTreeMap::from([
            (
                BETA_STORAGE_CLASS_ANNOTATION.to_string(),
                "legacy-class".to_string(),
            ),
            (SELECTED_NODE_ANNOTATION.to_string(), "worker-1".to_string()),
        ]));
        let pv = PersistentVolume {
            metadata: ObjectMeta { annotations: annotations.clone(), ..Default::default() },
            spec: Some(PersistentVolumeSpec {
                claim_ref: Some(ObjectReference {
                    namespace: Some("ns".to_string()),
                    name: Some("claim".to_string()),
                    uid: Some("claim-uid".to_string()),
                    ..Default::default()
                }),
                storage_class_name: Some("spec-class".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let pvc = PersistentVolumeClaim {
            metadata: ObjectMeta {
                namespace: Some("ns".to_string()),
                name: Some("claim".to_string()),
                uid: Some("claim-uid".to_string()),
                annotations,
                ..Default::default()
            },
            spec: Some(PersistentVolumeClaimSpec {
                storage_class_name: Some("spec-class".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        };

        let projected_pv = PvInfo::from_api(&pv);
        let projected_pvc = PvcInfo::from_api(&pvc);
        assert!(projected_pv.claim_ref_present);
        assert_eq!(projected_pv.claim_ref_uid.as_deref(), Some("claim-uid"));
        assert_eq!(projected_pvc.uid, "claim-uid");
        assert_eq!(projected_pv.storage_class_name, "legacy-class");
        assert_eq!(projected_pvc.storage_class_name.as_deref(), Some("legacy-class"));
        assert_eq!(projected_pvc.selected_node.as_deref(), Some("worker-1"));
    }
}
