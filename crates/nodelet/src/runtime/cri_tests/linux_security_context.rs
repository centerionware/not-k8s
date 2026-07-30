//! linux_security_context(): translates pod/container securityContext into
//! CRI's LinuxContainerSecurityContext. Before this, securityContext was
//! ignored completely — runAsUser, capabilities, privileged, readOnlyRootFilesystem,
//! and seccomp all had no effect regardless of what the Pod spec said.
use super::*;
use k8s_openapi::api::core::v1::{Capabilities, SeccompProfile};

#[test]
fn nothing_set_anywhere_produces_all_defaults() {
    let sc = linux_security_context(None, None);
    assert_eq!(sc.run_as_user, None);
    assert_eq!(sc.run_as_group, None);
    assert!(!sc.privileged);
    assert!(!sc.readonly_rootfs);
    assert!(!sc.no_new_privs);
    assert_eq!(sc.capabilities, None);
    assert!(sc.supplemental_groups.is_empty());
    assert_eq!(sc.seccomp, None);
}

#[test]
fn container_level_run_as_user_is_applied() {
    let csc = SecurityContext { run_as_user: Some(1000), ..Default::default() };
    let sc = linux_security_context(None, Some(&csc));
    assert_eq!(sc.run_as_user, Some(Int64Value { value: 1000 }));
}

#[test]
fn container_level_overrides_pod_level_run_as_user() {
    let psc = PodSecurityContext { run_as_user: Some(1), ..Default::default() };
    let csc = SecurityContext { run_as_user: Some(2000), ..Default::default() };
    let sc = linux_security_context(Some(&psc), Some(&csc));
    assert_eq!(sc.run_as_user, Some(Int64Value { value: 2000 }));
}

#[test]
fn pod_level_run_as_user_applies_when_container_doesnt_override() {
    let psc = PodSecurityContext { run_as_user: Some(1), ..Default::default() };
    let sc = linux_security_context(Some(&psc), None);
    assert_eq!(sc.run_as_user, Some(Int64Value { value: 1 }));
}

#[test]
fn privileged_true_is_carried_through() {
    let csc = SecurityContext { privileged: Some(true), ..Default::default() };
    assert!(linux_security_context(None, Some(&csc)).privileged);
}

#[test]
fn read_only_root_filesystem_is_carried_through() {
    let csc = SecurityContext { read_only_root_filesystem: Some(true), ..Default::default() };
    assert!(linux_security_context(None, Some(&csc)).readonly_rootfs);
}

#[test]
fn allow_privilege_escalation_false_sets_no_new_privs() {
    let csc = SecurityContext { allow_privilege_escalation: Some(false), ..Default::default() };
    assert!(linux_security_context(None, Some(&csc)).no_new_privs);
}

#[test]
fn allow_privilege_escalation_unset_or_true_does_not_set_no_new_privs() {
    assert!(!linux_security_context(None, None).no_new_privs);
    let csc = SecurityContext { allow_privilege_escalation: Some(true), ..Default::default() };
    assert!(!linux_security_context(None, Some(&csc)).no_new_privs);
}

#[test]
fn capabilities_add_and_drop_are_translated() {
    let csc = SecurityContext {
        capabilities: Some(Capabilities {
            add: Some(vec!["NET_ADMIN".to_string()]),
            drop: Some(vec!["ALL".to_string()]),
        }),
        ..Default::default()
    };
    let sc = linux_security_context(None, Some(&csc));
    let caps = sc.capabilities.unwrap();
    assert_eq!(caps.add_capabilities, vec!["NET_ADMIN".to_string()]);
    assert_eq!(caps.drop_capabilities, vec!["ALL".to_string()]);
}

#[test]
fn supplemental_groups_come_from_pod_level_only() {
    let psc = PodSecurityContext { supplemental_groups: Some(vec![100, 200]), ..Default::default() };
    let sc = linux_security_context(Some(&psc), None);
    assert_eq!(sc.supplemental_groups, vec![100, 200]);
}

#[test]
fn seccomp_runtime_default_maps_to_runtime_default_profile_type() {
    let csc = SecurityContext {
        seccomp_profile: Some(SeccompProfile { type_: "RuntimeDefault".to_string(), ..Default::default() }),
        ..Default::default()
    };
    let sc = linux_security_context(None, Some(&csc));
    assert_eq!(sc.seccomp.unwrap().profile_type, ProfileType::RuntimeDefault as i32);
}

#[test]
fn seccomp_localhost_carries_the_profile_path() {
    let csc = SecurityContext {
        seccomp_profile: Some(SeccompProfile {
            type_: "Localhost".to_string(),
            localhost_profile: Some("profiles/my-profile.json".to_string()),
        }),
        ..Default::default()
    };
    let sc = linux_security_context(None, Some(&csc));
    let seccomp = sc.seccomp.unwrap();
    assert_eq!(seccomp.profile_type, ProfileType::Localhost as i32);
    assert_eq!(seccomp.localhost_ref, "profiles/my-profile.json");
}

#[test]
fn seccomp_container_level_overrides_pod_level() {
    let psc = PodSecurityContext {
        seccomp_profile: Some(SeccompProfile { type_: "RuntimeDefault".to_string(), ..Default::default() }),
        ..Default::default()
    };
    let csc = SecurityContext {
        seccomp_profile: Some(SeccompProfile { type_: "Unconfined".to_string(), ..Default::default() }),
        ..Default::default()
    };
    let sc = linux_security_context(Some(&psc), Some(&csc));
    assert_eq!(sc.seccomp.unwrap().profile_type, ProfileType::Unconfined as i32);
}

#[test]
fn no_seccomp_profile_anywhere_leaves_it_unset() {
    assert_eq!(linux_security_context(None, None).seccomp, None);
}
