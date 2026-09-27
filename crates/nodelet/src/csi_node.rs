//! Reconciles this node's `CSINode.spec.drivers[]` — real kubelet's own
//! "Node Info Manager" does the same thing, driven by the exact same
//! signal: a CSI driver registering via the plugin-registration protocol
//! (`plugin_registry.rs`). Without this, `CSINode.spec.drivers` stays
//! empty forever, and a topology-aware external-provisioner
//! (`--feature-gates=Topology=true` — the common default for real CSI
//! drivers, including this codebase's own bundled deploy manifest) walks
//! every `CSINode` object looking for the registering driver's entry,
//! never finds one, and permanently fails every `CreateVolume` with
//! "no available topology found" — found live testing a real CSI driver's
//! PVC provisioning, after rounds 117-119 already got the driver's own
//! pod running cleanly.
//!
//! This reconciles `spec.drivers[].{name,nodeID,topologyKeys}` here; the
//! other half — syncing `NodeGetInfo`'s `accessible_topology` *segment
//! values* onto the Node object's own labels — turned out not to be
//! optional after all: live testing showed `csi-provisioner
//! --feature-gates=Topology=true` reads `topologyKeys` off `CSINode` to
//! know which label *keys* matter, then reads the values straight off the
//! Node object itself to build `TopologyRequirement`, and fails
//! provisioning ("topologyKeys [...] were not found on any nodes") without
//! both. See `node.rs`'s `apply_topology_labels()` for that half.

use anyhow::{Context, Result};
use k8s_openapi::api::core::v1::Node;
use k8s_openapi::api::storage::v1::{CSINode, CSINodeDriver, CSINodeSpec};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference;
use kube::api::{Api, Patch, PatchParams, PostParams};
use kube::Client;

/// Pure list update: replace any existing entry with the same driver
/// name (a re-registration, e.g. after the driver's pod restarted) and
/// append the new one — pulled out so the "same name wins, order of the
/// rest doesn't matter" behavior is unit-testable without a real API
/// server.
pub(crate) fn upsert_driver(mut drivers: Vec<CSINodeDriver>, name: &str, node_id: &str, topology_keys: Vec<String>) -> Vec<CSINodeDriver> {
    drivers.retain(|d| d.name != name);
    drivers.push(CSINodeDriver {
        name: name.to_string(),
        node_id: node_id.to_string(),
        topology_keys: (!topology_keys.is_empty()).then_some(topology_keys),
        allocatable: None,
    });
    drivers
}

/// Pure list update: drop the named driver's entry (deregistration —
/// its socket disappeared, so its node info can no longer be assumed
/// current either). A name that isn't present is a harmless no-op, same
/// as `runtime/csi::CsiDrivers::deregister()`'s own posture.
pub(crate) fn remove_driver(mut drivers: Vec<CSINodeDriver>, name: &str) -> Vec<CSINodeDriver> {
    drivers.retain(|d| d.name != name);
    drivers
}

/// Fetch this node's `CSINode` object, or `None` if it doesn't exist yet
/// (a from-scratch cluster where nothing has created one — real kubelet
/// creates it lazily itself in that case, matched by `upsert()` below).
async fn get(client: &Client, node_name: &str) -> Result<Option<CSINode>> {
    let api: Api<CSINode> = Api::all(client.clone());
    match api.get(node_name).await {
        Ok(n) => Ok(Some(n)),
        Err(kube::Error::Api(e)) if e.code == 404 => Ok(None),
        Err(e) => Err(e).context("getting CSINode"),
    }
}

fn node_owner_reference(node_name: &str, node_uid: &str) -> OwnerReference {
    OwnerReference {
        api_version: "v1".to_string(),
        kind: "Node".to_string(),
        name: node_name.to_string(),
        uid: node_uid.to_string(),
        ..Default::default()
    }
}

async fn current_node_owner_reference(client: &Client, node_name: &str) -> Result<OwnerReference> {
    let nodes: Api<Node> = Api::all(client.clone());
    let node = nodes
        .get(node_name)
        .await
        .with_context(|| format!("getting Node {node_name} for CSINode owner reference"))?;
    let uid = node
        .metadata
        .uid
        .filter(|uid| !uid.is_empty())
        .with_context(|| format!("Node {node_name} has no UID for CSINode owner reference"))?;
    Ok(node_owner_reference(node_name, &uid))
}

/// Register (or update) `driver`'s entry on this node's `CSINode` object,
/// creating the object itself if this cluster doesn't have one yet (a k3s
/// control plane creates it automatically the moment a Node registers —
/// the common case this was found against — but a from-scratch setup
/// might not have anything else that does). Read-modify-write, same
/// "real kubelet's node info manager retries on conflict" shape, but
/// without the retry loop: a lost race here just means the next
/// `plugin_registry.rs` sync tick (a few seconds later) reconciles it
/// again, same self-healing posture `runtime/csi::CsiDrivers`'s own
/// in-memory state already has.
pub async fn upsert(client: &Client, node_name: &str, driver: &str, node_id: &str, topology_keys: Vec<String>) -> Result<()> {
    let api: Api<CSINode> = Api::all(client.clone());
    // Re-read the Node UID for every registration so same-name replacement
    // repairs a stale CSINode owner reference before garbage collection can
    // remove the freshly registered driver's node information.
    let owner_reference = current_node_owner_reference(client, node_name).await?;
    match get(client, node_name).await? {
        Some(existing) => {
            let drivers = upsert_driver(existing.spec.drivers, driver, node_id, topology_keys);
            let patch = serde_json::json!({
                "metadata": { "ownerReferences": [owner_reference] },
                "spec": { "drivers": drivers }
            });
            api.patch(node_name, &PatchParams::default(), &Patch::Merge(&patch)).await.context("patching CSINode")?;
        }
        None => {
            let csi_node = CSINode {
                metadata: kube::api::ObjectMeta {
                    name: Some(node_name.to_string()),
                    owner_references: Some(vec![owner_reference]),
                    ..Default::default()
                },
                spec: CSINodeSpec { drivers: upsert_driver(Vec::new(), driver, node_id, topology_keys) },
            };
            api.create(&PostParams::default(), &csi_node).await.context("creating CSINode")?;
        }
    }
    Ok(())
}

/// Remove `driver`'s entry from this node's `CSINode` object. A missing
/// `CSINode` object (never created, or already gone) is a harmless no-op
/// — there's nothing to remove an entry from.
pub async fn remove(client: &Client, node_name: &str, driver: &str) -> Result<()> {
    let api: Api<CSINode> = Api::all(client.clone());
    if let Some(existing) = get(client, node_name).await? {
        let drivers = remove_driver(existing.spec.drivers, driver);
        let patch = serde_json::json!({ "spec": { "drivers": drivers } });
        api.patch(node_name, &PatchParams::default(), &Patch::Merge(&patch)).await.context("patching CSINode")?;
    }
    Ok(())
}

#[cfg(test)]
#[path = "csi_node_tests/upsert_remove.rs"]
mod tests_upsert_remove;
