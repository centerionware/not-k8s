//! upsert_driver()/remove_driver(): pure CSINode.spec.drivers[] list
//! update logic, split out from the real API calls so it's testable
//! without a live apiserver. Regression coverage for the "no available
//! topology found" PVC-provisioning bug — see this module's own doc
//! comment for the full story.
use super::*;

fn driver(name: &str, node_id: &str) -> CSINodeDriver {
    CSINodeDriver { name: name.to_string(), node_id: node_id.to_string(), topology_keys: None, allocatable: None }
}

#[test]
fn upsert_into_an_empty_list_adds_the_entry() {
    let drivers = upsert_driver(Vec::new(), "hostpath.csi.k8s.io", "debian", Vec::new());
    assert_eq!(drivers.len(), 1);
    assert_eq!(drivers[0].name, "hostpath.csi.k8s.io");
    assert_eq!(drivers[0].node_id, "debian");
    assert_eq!(drivers[0].topology_keys, None);
}

#[test]
fn upsert_with_topology_keys_sets_them() {
    let drivers = upsert_driver(Vec::new(), "d1", "n1", vec!["zone".to_string()]);
    assert_eq!(drivers[0].topology_keys, Some(vec!["zone".to_string()]));
}

#[test]
fn upsert_leaves_other_drivers_alone() {
    let existing = vec![driver("other.csi.k8s.io", "n1")];
    let drivers = upsert_driver(existing, "hostpath.csi.k8s.io", "n2", Vec::new());
    assert_eq!(drivers.len(), 2);
    assert!(drivers.iter().any(|d| d.name == "other.csi.k8s.io"));
    assert!(drivers.iter().any(|d| d.name == "hostpath.csi.k8s.io"));
}

#[test]
fn upsert_replaces_an_existing_entry_for_the_same_driver_not_duplicates_it() {
    // A driver's pod restarting re-registers under the same name — this
    // must overwrite the stale entry (e.g. a changed node_id), not pile
    // up a second one.
    let existing = vec![driver("hostpath.csi.k8s.io", "old-node-id")];
    let drivers = upsert_driver(existing, "hostpath.csi.k8s.io", "new-node-id", Vec::new());
    assert_eq!(drivers.len(), 1);
    assert_eq!(drivers[0].node_id, "new-node-id");
}

#[test]
fn remove_drops_only_the_named_driver() {
    let existing = vec![driver("keep-me", "n1"), driver("remove-me", "n2")];
    let drivers = remove_driver(existing, "remove-me");
    assert_eq!(drivers.len(), 1);
    assert_eq!(drivers[0].name, "keep-me");
}

#[test]
fn remove_an_unknown_name_is_a_harmless_no_op() {
    let existing = vec![driver("keep-me", "n1")];
    let drivers = remove_driver(existing, "never-registered");
    assert_eq!(drivers.len(), 1);
}

#[test]
fn remove_from_an_empty_list_stays_empty() {
    assert!(remove_driver(Vec::new(), "anything").is_empty());
}
