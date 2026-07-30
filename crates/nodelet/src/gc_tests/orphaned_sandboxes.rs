use super::*;

fn set(keys: &[&str]) -> HashSet<String> {
    keys.iter().map(|s| s.to_string()).collect()
}

#[test]
fn sandbox_with_a_live_pod_is_kept() {
    let sandboxes = vec![("default".to_string(), "web".to_string(), "sb-1".to_string())];
    let live = set(&["default/web"]);
    assert!(orphaned_sandboxes(&sandboxes, &live).is_empty());
}

#[test]
fn sandbox_whose_pod_no_longer_exists_is_orphaned() {
    let sandboxes = vec![("default".to_string(), "web".to_string(), "sb-1".to_string())];
    let live = set(&["default/other"]);
    assert_eq!(orphaned_sandboxes(&sandboxes, &live), vec!["sb-1".to_string()]);
}

#[test]
fn mixed_live_and_orphaned_only_returns_the_orphans() {
    let sandboxes = vec![
        ("default".to_string(), "web".to_string(), "sb-1".to_string()),
        ("default".to_string(), "gone".to_string(), "sb-2".to_string()),
        ("kube-system".to_string(), "coredns".to_string(), "sb-3".to_string()),
    ];
    let live = set(&["default/web", "kube-system/coredns"]);
    assert_eq!(orphaned_sandboxes(&sandboxes, &live), vec!["sb-2".to_string()]);
}

#[test]
fn empty_sandbox_list_returns_empty() {
    assert!(orphaned_sandboxes(&[], &set(&["default/web"])).is_empty());
}

#[test]
fn same_pod_name_in_different_namespaces_are_distinct() {
    let sandboxes = vec![("ns-a".to_string(), "web".to_string(), "sb-1".to_string())];
    let live = set(&["ns-b/web"]); // same name, different namespace -> still orphaned
    assert_eq!(orphaned_sandboxes(&sandboxes, &live), vec!["sb-1".to_string()]);
}
