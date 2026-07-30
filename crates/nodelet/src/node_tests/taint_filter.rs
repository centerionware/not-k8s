//! taints_without(): the fix for pods staying Unschedulable forever
//! ("1 node(s) had untolerated taint(s)") — k3s's kube-controller-manager
//! taints every fresh Node with node.cloudprovider.kubernetes.io/uninitialized
//! regardless of --disable-cloud-controller, and nothing ever cleared it
//! before this. This is the pure filtering logic clear_cloudprovider_taint()
//! uses to decide what to send back in the patch.
use super::*;

fn taint(key: &str) -> Taint {
    Taint { key: key.to_string(), effect: "NoSchedule".to_string(), ..Default::default() }
}

#[test]
fn empty_taint_list_has_it_false_and_stays_empty() {
    let (kept, has_it) = taints_without(&[], CLOUDPROVIDER_TAINT_KEY);
    assert!(!has_it);
    assert!(kept.is_empty());
}

#[test]
fn matching_taint_is_detected_and_removed() {
    let taints = vec![taint(CLOUDPROVIDER_TAINT_KEY)];
    let (kept, has_it) = taints_without(&taints, CLOUDPROVIDER_TAINT_KEY);
    assert!(has_it);
    assert!(kept.is_empty());
}

#[test]
fn other_taints_are_preserved() {
    let taints = vec![
        taint("node-role.kubernetes.io/control-plane"),
        taint(CLOUDPROVIDER_TAINT_KEY),
        taint("some.other/taint"),
    ];
    let (kept, has_it) = taints_without(&taints, CLOUDPROVIDER_TAINT_KEY);
    assert!(has_it);
    assert_eq!(kept.len(), 2);
    assert!(kept.iter().any(|t| t.key == "node-role.kubernetes.io/control-plane"));
    assert!(kept.iter().any(|t| t.key == "some.other/taint"));
    assert!(!kept.iter().any(|t| t.key == CLOUDPROVIDER_TAINT_KEY));
}

#[test]
fn absent_taint_leaves_list_untouched_and_has_it_false() {
    let taints = vec![taint("node-role.kubernetes.io/control-plane")];
    let (kept, has_it) = taints_without(&taints, CLOUDPROVIDER_TAINT_KEY);
    assert!(!has_it);
    assert_eq!(kept.len(), 1);
}

#[test]
fn duplicate_matching_taints_are_all_removed() {
    // Shouldn't happen in a real Node object, but the filter must not stop
    // at the first match.
    let taints = vec![taint(CLOUDPROVIDER_TAINT_KEY), taint(CLOUDPROVIDER_TAINT_KEY)];
    let (kept, has_it) = taints_without(&taints, CLOUDPROVIDER_TAINT_KEY);
    assert!(has_it);
    assert!(kept.is_empty());
}
