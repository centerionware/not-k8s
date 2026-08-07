//! linux_security_context(): translates pod/container securityContext into
//! CRI's LinuxContainerSecurityContext. Before this, securityContext was
//! ignored completely — runAsUser, capabilities, privileged, readOnlyRootFilesystem,
//! and seccomp all had no effect regardless of what the Pod spec said.
use super::*;
use k8s_openapi::api::core::v1::{Capabilities, SeccompProfile};

#[test]
fn nothing_set_anywhere_produces_all_defaults() {
    let sc = linux_security_context(None, None, NamespaceMode::Container, None);
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
    let sc = linux_security_context(None, Some(&csc), NamespaceMode::Container, None);
    assert_eq!(sc.run_as_user, Some(Int64Value { value: 1000 }));
}

#[test]
fn container_level_overrides_pod_level_run_as_user() {
    let psc = PodSecurityContext { run_as_user: Some(1), ..Default::default() };
    let csc = SecurityContext { run_as_user: Some(2000), ..Default::default() };
    let sc = linux_security_context(Some(&psc), Some(&csc), NamespaceMode::Container, None);
    assert_eq!(sc.run_as_user, Some(Int64Value { value: 2000 }));
}

#[test]
fn pod_level_run_as_user_applies_when_container_doesnt_override() {
    let psc = PodSecurityContext { run_as_user: Some(1), ..Default::default() };
    let sc = linux_security_context(Some(&psc), None, NamespaceMode::Container, None);
    assert_eq!(sc.run_as_user, Some(Int64Value { value: 1 }));
}

#[test]
fn privileged_true_is_carried_through() {
    let csc = SecurityContext { privileged: Some(true), ..Default::default() };
    assert!(linux_security_context(None, Some(&csc), NamespaceMode::Container, None).privileged);
}

#[test]
fn read_only_root_filesystem_is_carried_through() {
    let csc = SecurityContext { read_only_root_filesystem: Some(true), ..Default::default() };
    assert!(linux_security_context(None, Some(&csc), NamespaceMode::Container, None).readonly_rootfs);
}

#[test]
fn allow_privilege_escalation_false_sets_no_new_privs() {
    let csc = SecurityContext { allow_privilege_escalation: Some(false), ..Default::default() };
    assert!(linux_security_context(None, Some(&csc), NamespaceMode::Container, None).no_new_privs);
}

#[test]
fn allow_privilege_escalation_unset_or_true_does_not_set_no_new_privs() {
    assert!(!linux_security_context(None, None, NamespaceMode::Container, None).no_new_privs);
    let csc = SecurityContext { allow_privilege_escalation: Some(true), ..Default::default() };
    assert!(!linux_security_context(None, Some(&csc), NamespaceMode::Container, None).no_new_privs);
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
    let sc = linux_security_context(None, Some(&csc), NamespaceMode::Container, None);
    let caps = sc.capabilities.unwrap();
    assert_eq!(caps.add_capabilities, vec!["NET_ADMIN".to_string()]);
    assert_eq!(caps.drop_capabilities, vec!["ALL".to_string()]);
}

#[test]
fn supplemental_groups_come_from_pod_level_only() {
    let psc = PodSecurityContext { supplemental_groups: Some(vec![100, 200]), ..Default::default() };
    let sc = linux_security_context(Some(&psc), None, NamespaceMode::Container, None);
    assert_eq!(sc.supplemental_groups, vec![100, 200]);
}

#[test]
fn seccomp_runtime_default_maps_to_runtime_default_profile_type() {
    let csc = SecurityContext {
        seccomp_profile: Some(SeccompProfile { type_: "RuntimeDefault".to_string(), ..Default::default() }),
        ..Default::default()
    };
    let sc = linux_security_context(None, Some(&csc), NamespaceMode::Container, None);
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
    let sc = linux_security_context(None, Some(&csc), NamespaceMode::Container, None);
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
    let sc = linux_security_context(Some(&psc), Some(&csc), NamespaceMode::Container, None);
    assert_eq!(sc.seccomp.unwrap().profile_type, ProfileType::Unconfined as i32);
}

#[test]
fn no_seccomp_profile_anywhere_leaves_it_unset() {
    assert_eq!(linux_security_context(None, None, NamespaceMode::Container, None).seccomp, None);
}

// --- supplementalGroupsPolicy (round 62) ---

#[test]
fn no_pod_security_context_defaults_to_merge() {
    let sc = linux_security_context(None, None, NamespaceMode::Container, None);
    assert_eq!(sc.supplemental_groups_policy, v1::SupplementalGroupsPolicy::Merge as i32);
}

#[test]
fn explicit_strict_is_translated() {
    let psc = PodSecurityContext { supplemental_groups_policy: Some("Strict".to_string()), ..Default::default() };
    let sc = linux_security_context(Some(&psc), None, NamespaceMode::Container, None);
    assert_eq!(sc.supplemental_groups_policy, v1::SupplementalGroupsPolicy::Strict as i32);
}

#[test]
fn an_unrecognized_value_falls_back_to_merge() {
    // Real k8s apiserver validation only ever allows "Merge"/"Strict", but
    // this must fail open rather than panic/misbehave on anything else.
    let psc = PodSecurityContext { supplemental_groups_policy: Some("Bogus".to_string()), ..Default::default() };
    let sc = linux_security_context(Some(&psc), None, NamespaceMode::Container, None);
    assert_eq!(sc.supplemental_groups_policy, v1::SupplementalGroupsPolicy::Merge as i32);
}

// --- pid namespace_options (round 40) ---

#[test]
fn pid_mode_is_carried_through_into_namespace_options() {
    let sc = linux_security_context(None, None, NamespaceMode::Pod, None);
    assert_eq!(sc.namespace_options.unwrap().pid, NamespaceMode::Pod as i32);
}

#[test]
fn container_scoped_pid_mode_is_the_default_when_nothing_special_applies() {
    let sc = linux_security_context(None, None, NamespaceMode::Container, None);
    assert_eq!(sc.namespace_options.unwrap().pid, NamespaceMode::Container as i32);
}

// --- userns_options (round 123; found live in CI) ---

#[test]
fn no_userns_mapping_means_no_userns_options_on_the_container() {
    let sc = linux_security_context(None, None, NamespaceMode::Container, None);
    assert!(sc.namespace_options.unwrap().userns_options.is_none());
}

#[test]
fn a_userns_mapping_is_carried_onto_the_container_same_as_the_sandbox() {
    // Round 123: linux_security_context() used to build its NamespaceOption
    // with only `pid` set, leaving userns_options unset even for a
    // hostUsers: false pod — confirmed live via an OCI-spec dump that the
    // sandbox got a real user namespace but the app container silently
    // didn't join it, still seeing the host's own full identity range.
    // Every container must get the identical mapping sandbox_config()
    // already sends for the sandbox, or CreateContainer defaults it to
    // the host's own (no) user namespace instead.
    let sc = linux_security_context(None, None, NamespaceMode::Container, Some((100_000, 65_536)));
    let userns = sc.namespace_options.unwrap().userns_options.unwrap();
    assert_eq!(userns.mode, NamespaceMode::Pod as i32);
    assert_eq!(userns.uids.len(), 1);
    assert_eq!(userns.uids[0].host_id, 100_000);
    assert_eq!(userns.uids[0].container_id, 0);
    assert_eq!(userns.uids[0].length, 65_536);
    assert_eq!(userns.gids, userns.uids, "uid and gid mappings should mirror the same range");
}

// --- procMount (round 78; found in round 76's re-audit) ---

#[test]
fn no_proc_mount_set_gets_the_default_masked_and_readonly_paths() {
    // The real-world-relevant case: without this, nothing was ever sent
    // at all, and a modern containerd (disable_proc_mount=false, its own
    // default config) applies NO masking whatsoever when the field is
    // absent -- a silent security regression this round closes, not just
    // a missing Unmasked toggle.
    let sc = linux_security_context(None, None, NamespaceMode::Container, None);
    assert!(!sc.masked_paths.is_empty());
    assert!(sc.masked_paths.contains(&"/proc/acpi".to_string()));
    assert!(!sc.readonly_paths.is_empty());
    assert!(sc.readonly_paths.contains(&"/proc/sys".to_string()));
}

#[test]
fn explicit_default_proc_mount_gets_the_same_default_paths() {
    let csc = SecurityContext { proc_mount: Some("Default".to_string()), ..Default::default() };
    let sc = linux_security_context(None, Some(&csc), NamespaceMode::Container, None);
    assert!(!sc.masked_paths.is_empty());
    assert!(!sc.readonly_paths.is_empty());
}

#[test]
fn unmasked_proc_mount_gets_genuinely_empty_masked_and_readonly_paths() {
    let csc = SecurityContext { proc_mount: Some("Unmasked".to_string()), ..Default::default() };
    let sc = linux_security_context(None, Some(&csc), NamespaceMode::Container, None);
    assert!(sc.masked_paths.is_empty());
    assert!(sc.readonly_paths.is_empty());
}
