//! pid_namespace_mode(): hostPID/shareProcessNamespace precedence (Round 40;
//! found in round 39's re-audit).
use super::*;

#[test]
fn defaults_to_container_scoped() {
    assert_eq!(pid_namespace_mode(false, false), NamespaceMode::Container);
}

#[test]
fn share_process_namespace_alone_is_pod_scoped() {
    assert_eq!(pid_namespace_mode(false, true), NamespaceMode::Pod);
}

#[test]
fn host_pid_alone_is_node_scoped() {
    assert_eq!(pid_namespace_mode(true, false), NamespaceMode::Node);
}

#[test]
fn host_pid_wins_over_share_process_namespace() {
    assert_eq!(pid_namespace_mode(true, true), NamespaceMode::Node);
}
