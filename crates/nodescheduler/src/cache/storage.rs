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
use k8s_openapi::api::storage::v1::{CSIDriver, CSINode, CSIStorageCapacity, StorageClass};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::LabelSelector;
use std::collections::BTreeMap;

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
}

impl PvInfo {
    pub fn from_api(pv: &PersistentVolume) -> Self {
        let spec = pv.spec.clone().unwrap_or_default();
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
            storage_class_name: spec.storage_class_name.unwrap_or_default(),
            node_affinity: spec.node_affinity.and_then(|na| na.required).map(Box::new),
            labels: pv.metadata.labels.clone().unwrap_or_default(),
            csi_driver: spec.csi.map(|c| c.driver),
        }
    }
}

/// A PersistentVolumeClaim, projected.
#[derive(Clone, Debug, Default)]
pub struct PvcInfo {
    pub namespace: String,
    pub name: String,
    pub storage_class_name: Option<String>,
    /// `spec.volumeName` — set once bound (or pre-bound by an admin/user
    /// pointing a claim at a specific PV).
    pub volume_name: Option<String>,
    pub requested_access_modes: Vec<String>,
    pub requested_bytes: i64,
    pub selector: Option<LabelSelector>,
    /// `status.phase == "Bound"`.
    pub bound: bool,
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
        let requested_bytes = spec
            .resources
            .as_ref()
            .and_then(|r| r.requests.as_ref())
            .and_then(|r| r.get("storage"))
            .map(quantity_bytes)
            .unwrap_or(0);
        let bound = pvc.status.as_ref().and_then(|s| s.phase.as_deref()) == Some("Bound");
        PvcInfo {
            namespace: pvc.metadata.namespace.clone().unwrap_or_default(),
            name: pvc.metadata.name.clone().unwrap_or_default(),
            storage_class_name: spec.storage_class_name,
            volume_name: spec.volume_name,
            requested_access_modes: spec.access_modes.unwrap_or_default(),
            requested_bytes,
            selector: spec.selector,
            bound,
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
