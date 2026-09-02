#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ns_with_level(level: &str) -> Value {
        json!({"metadata": {"labels": {"pod-security.kubernetes.io/enforce": level}}})
    }

    #[test]
    fn enforcement_level_reads_the_real_label() {
        assert_eq!(
            enforcement_level(&ns_with_level("baseline")),
            Level::Baseline
        );
        assert_eq!(
            enforcement_level(&ns_with_level("restricted")),
            Level::Restricted
        );
        assert_eq!(
            enforcement_level(&ns_with_level("privileged")),
            Level::Privileged
        );
    }

    #[test]
    fn enforcement_level_defaults_to_privileged_when_absent_or_unrecognized() {
        assert_eq!(enforcement_level(&json!({})), Level::Privileged);
        assert_eq!(
            enforcement_level(&ns_with_level("something-future")),
            Level::Privileged
        );
    }

    #[test]
    fn privileged_level_enforces_nothing() {
        let pod = json!({"spec": {"hostNetwork": true, "containers": [{"name": "c1", "securityContext": {"privileged": true}}]}});
        assert!(validate(&pod, Level::Privileged).is_empty());
    }

    #[test]
    fn baseline_rejects_a_privileged_container() {
        let pod = json!({"spec": {"containers": [{"name": "c1", "securityContext": {"privileged": true}}]}});
        let violations = validate(&pod, Level::Baseline);
        assert_eq!(violations.len(), 1);
        assert!(violations[0].contains("privileged"));
    }

    #[test]
    fn baseline_rejects_host_namespaces() {
        for field in ["hostNetwork", "hostPID", "hostIPC"] {
            let pod = json!({"spec": {field: true, "containers": []}});
            let violations = validate(&pod, Level::Baseline);
            assert_eq!(violations.len(), 1, "{field} must be rejected");
        }
    }

    #[test]
    fn baseline_rejects_a_host_port() {
        let pod = json!({"spec": {"containers": [{"name": "c1", "ports": [{"hostPort": 8080}]}]}});
        let violations = validate(&pod, Level::Baseline);
        assert_eq!(violations.len(), 1);
        assert!(violations[0].contains("8080"));
    }

    #[test]
    fn baseline_allows_a_container_port_with_no_host_port() {
        let pod =
            json!({"spec": {"containers": [{"name": "c1", "ports": [{"containerPort": 8080}]}]}});
        assert!(validate(&pod, Level::Baseline).is_empty());
    }

    #[test]
    fn baseline_rejects_a_hostpath_volume() {
        let pod = json!({"spec": {"containers": [], "volumes": [{"name": "v1", "hostPath": {"path": "/etc"}}]}});
        let violations = validate(&pod, Level::Baseline);
        assert_eq!(violations.len(), 1);
        assert!(violations[0].contains("v1"));
    }

    #[test]
    fn baseline_allows_a_non_hostpath_volume() {
        let pod = json!({"spec": {"containers": [], "volumes": [{"name": "v1", "emptyDir": {}}]}});
        assert!(validate(&pod, Level::Baseline).is_empty());
    }

    #[test]
    fn baseline_rejects_a_non_default_capability() {
        let pod = json!({"spec": {"containers": [{"name": "c1", "securityContext": {"capabilities": {"add": ["NET_ADMIN"]}}}]}});
        let violations = validate(&pod, Level::Baseline);
        assert_eq!(violations.len(), 1);
        assert!(violations[0].contains("NET_ADMIN"));
    }

    #[test]
    fn baseline_allows_a_default_capability() {
        let pod = json!({"spec": {"containers": [{"name": "c1", "securityContext": {"capabilities": {"add": ["CHOWN"]}}}]}});
        assert!(validate(&pod, Level::Baseline).is_empty());
    }

    #[test]
    fn baseline_rejects_an_unconfined_seccomp_profile() {
        let pod = json!({"spec": {"containers": [], "securityContext": {"seccompProfile": {"type": "Unconfined"}}}});
        let violations = validate(&pod, Level::Baseline);
        assert_eq!(violations.len(), 1);
        assert!(violations[0].contains("Unconfined"));
    }

    #[test]
    fn baseline_allows_runtime_default_seccomp() {
        let pod = json!({"spec": {"containers": [], "securityContext": {"seccompProfile": {"type": "RuntimeDefault"}}}});
        assert!(validate(&pod, Level::Baseline).is_empty());
    }

    #[test]
    fn baseline_allows_a_clean_pod() {
        let pod = json!({"spec": {"containers": [{"name": "c1", "image": "nginx"}]}});
        assert!(validate(&pod, Level::Baseline).is_empty());
    }

    #[test]
    fn multiple_violations_are_all_collected_not_just_the_first() {
        let pod = json!({"spec": {
            "hostNetwork": true,
            "containers": [{"name": "c1", "securityContext": {"privileged": true}}],
        }});
        let violations = validate(&pod, Level::Baseline);
        assert_eq!(violations.len(), 2);
    }

    #[test]
    fn init_containers_are_checked_too() {
        let pod = json!({"spec": {"initContainers": [{"name": "init1", "securityContext": {"privileged": true}}]}});
        let violations = validate(&pod, Level::Baseline);
        assert_eq!(violations.len(), 1);
        assert!(violations[0].contains("init1"));
    }

    #[test]
    fn baseline_rejects_a_forbidden_sysctl() {
        let pod = json!({"spec": {"containers": [], "securityContext": {"sysctls": [{"name": "kernel.msgmax", "value": "1"}]}}});
        let violations = validate(&pod, Level::Baseline);
        assert_eq!(violations.len(), 1);
        assert!(violations[0].contains("kernel.msgmax"));
    }

    #[test]
    fn baseline_allows_a_safe_sysctl() {
        let pod = json!({"spec": {"containers": [], "securityContext": {"sysctls": [{"name": "net.ipv4.tcp_syncookies", "value": "1"}]}}});
        assert!(validate(&pod, Level::Baseline).is_empty());
    }

    #[test]
    fn baseline_rejects_a_non_default_proc_mount() {
        let pod = json!({"spec": {"containers": [{"name": "c1", "securityContext": {"procMount": "Unmasked"}}]}});
        let violations = validate(&pod, Level::Baseline);
        assert_eq!(violations.len(), 1);
        assert!(violations[0].contains("Unmasked"));
    }

    #[test]
    fn proc_mount_is_unenforced_for_a_user_namespace_pod() {
        let pod = json!({"spec": {"hostUsers": false, "containers": [{"name": "c1", "securityContext": {"procMount": "Unmasked"}}]}});
        assert!(validate(&pod, Level::Baseline).is_empty());
    }

    #[test]
    fn baseline_rejects_a_probe_with_a_host_field() {
        let pod = json!({"spec": {"containers": [{"name": "c1", "livenessProbe": {"httpGet": {"host": "169.254.169.254", "path": "/"}}}]}});
        let violations = validate(&pod, Level::Baseline);
        assert_eq!(violations.len(), 1);
        assert!(violations[0].contains("169.254.169.254"));
    }

    #[test]
    fn baseline_allows_a_probe_with_no_host_field() {
        let pod = json!({"spec": {"containers": [{"name": "c1", "livenessProbe": {"httpGet": {"path": "/"}}}]}});
        assert!(validate(&pod, Level::Baseline).is_empty());
    }

    #[test]
    fn baseline_rejects_a_lifecycle_handler_with_a_host_field() {
        let pod = json!({"spec": {"containers": [{"name": "c1", "lifecycle": {"preStop": {"httpGet": {"host": "evil", "path": "/"}}}}]}});
        let violations = validate(&pod, Level::Baseline);
        assert_eq!(violations.len(), 1);
    }

    #[test]
    fn baseline_rejects_windows_host_process() {
        let pod = json!({"spec": {"containers": [{"name": "c1", "securityContext": {"windowsOptions": {"hostProcess": true}}}]}});
        let violations = validate(&pod, Level::Baseline);
        assert_eq!(violations.len(), 1);
        assert!(violations[0].contains("hostProcess"));
    }

    #[test]
    fn baseline_rejects_a_pod_level_windows_host_process() {
        let pod = json!({"spec": {"containers": [], "securityContext": {"windowsOptions": {"hostProcess": true}}}});
        let violations = validate(&pod, Level::Baseline);
        assert_eq!(violations.len(), 1);
        assert!(violations[0].contains("pod"));
    }

    #[test]
    fn baseline_rejects_an_unconfined_apparmor_profile() {
        let pod = json!({"spec": {"containers": [{"name": "c1", "securityContext": {"appArmorProfile": {"type": "Unconfined"}}}]}});
        let violations = validate(&pod, Level::Baseline);
        assert_eq!(violations.len(), 1);
        assert!(violations[0].contains("Unconfined"));
    }

    #[test]
    fn baseline_allows_runtime_default_apparmor() {
        let pod = json!({"spec": {"containers": [{"name": "c1", "securityContext": {"appArmorProfile": {"type": "RuntimeDefault"}}}]}});
        assert!(validate(&pod, Level::Baseline).is_empty());
    }

    #[test]
    fn baseline_rejects_a_forbidden_apparmor_annotation() {
        let pod = json!({
            "metadata": {"annotations": {"container.apparmor.security.beta.kubernetes.io/c1": "unconfined"}},
            "spec": {"containers": [{"name": "c1"}]},
        });
        let violations = validate(&pod, Level::Baseline);
        assert_eq!(violations.len(), 1);
    }

    #[test]
    fn baseline_rejects_a_custom_selinux_type() {
        let pod = json!({"spec": {"containers": [{"name": "c1", "securityContext": {"seLinuxOptions": {"type": "spc_t"}}}]}});
        let violations = validate(&pod, Level::Baseline);
        assert_eq!(violations.len(), 1);
        assert!(violations[0].contains("spc_t"));
    }

    #[test]
    fn baseline_allows_an_approved_selinux_type() {
        let pod = json!({"spec": {"containers": [{"name": "c1", "securityContext": {"seLinuxOptions": {"type": "container_t"}}}]}});
        assert!(validate(&pod, Level::Baseline).is_empty());
    }

    #[test]
    fn baseline_rejects_a_selinux_user_or_role() {
        let pod = json!({"spec": {"containers": [{"name": "c1", "securityContext": {"seLinuxOptions": {"user": "custom_u"}}}]}});
        let violations = validate(&pod, Level::Baseline);
        assert_eq!(violations.len(), 1);
        assert!(violations[0].contains("user may not be set"));
    }

    #[test]
    fn restricted_rejects_a_container_that_does_not_opt_into_run_as_non_root() {
        let pod = json!({"spec": {"containers": [{"name": "c1"}]}});
        let violations = validate(&pod, Level::Restricted);
        assert!(violations.iter().any(|v| v.contains("runAsNonRoot")));
    }

    #[test]
    fn restricted_allows_run_as_non_root_set_at_pod_level() {
        let pod = json!({"spec": {"securityContext": {"runAsNonRoot": true}, "containers": [{"name": "c1"}]}});
        let violations = validate(&pod, Level::Restricted);
        assert!(!violations.iter().any(|v| v.contains("runAsNonRoot")));
    }

    #[test]
    fn restricted_rejects_run_as_user_zero() {
        let pod = json!({"spec": {"securityContext": {"runAsNonRoot": true}, "containers": [{"name": "c1", "securityContext": {"runAsUser": 0}}]}});
        let violations = validate(&pod, Level::Restricted);
        assert!(violations.iter().any(|v| v.contains("runAsUser=0")));
    }

    #[test]
    fn restricted_rejects_a_container_without_allow_privilege_escalation_false() {
        let pod = json!({"spec": {"securityContext": {"runAsNonRoot": true}, "containers": [{"name": "c1"}]}});
        let violations = validate(&pod, Level::Restricted);
        assert!(violations
            .iter()
            .any(|v| v.contains("allowPrivilegeEscalation")));
    }

    #[test]
    fn restricted_requires_dropping_all_capabilities() {
        let pod = json!({"spec": {"securityContext": {"runAsNonRoot": true}, "containers": [{"name": "c1", "securityContext": {"allowPrivilegeEscalation": false}}]}});
        let violations = validate(&pod, Level::Restricted);
        assert!(violations.iter().any(|v| v.contains(r#"drop=["ALL"]"#)));
    }

    #[test]
    fn restricted_allows_adding_net_bind_service() {
        let pod = json!({"spec": {"containers": [{"name": "c1", "securityContext": {"capabilities": {"drop": ["ALL"], "add": ["NET_BIND_SERVICE"]}}}]}});
        let violations = validate(&pod, Level::Restricted);
        assert!(!violations.iter().any(|v| v.contains("capabilities")));
    }

    #[test]
    fn restricted_rejects_adding_a_forbidden_capability() {
        let pod = json!({"spec": {"containers": [{"name": "c1", "securityContext": {"capabilities": {"drop": ["ALL"], "add": ["NET_ADMIN"]}}}]}});
        let violations = validate(&pod, Level::Restricted);
        assert!(violations.iter().any(|v| v.contains("NET_ADMIN")));
    }

    #[test]
    fn restricted_requires_a_seccomp_profile() {
        let pod = json!({"spec": {"securityContext": {"runAsNonRoot": true}, "containers": [{"name": "c1", "securityContext": {"allowPrivilegeEscalation": false, "capabilities": {"drop": ["ALL"]}}}]}});
        let violations = validate(&pod, Level::Restricted);
        assert!(violations.iter().any(|v| v.contains("seccompProfile")));
    }

    #[test]
    fn restricted_rejects_a_disallowed_volume_type() {
        let pod = json!({"spec": {"containers": [], "volumes": [{"name": "v1", "nfs": {"server": "1.2.3.4", "path": "/"}}]}});
        let violations = validate(&pod, Level::Restricted);
        assert!(violations
            .iter()
            .any(|v| v.contains("restricted volume type")));
    }

    #[test]
    fn restricted_allows_a_projected_volume() {
        let pod = json!({"spec": {"containers": [], "volumes": [{"name": "v1", "projected": {"sources": []}}]}});
        let violations = validate(&pod, Level::Restricted);
        assert!(!violations.iter().any(|v| v.contains("volume type")));
    }

    #[test]
    fn restricted_does_not_double_report_a_hostpath_volume() {
        // hostPathVolumes is overridden by restrictedVolumes at the
        // Restricted level -- exactly one violation for this, not two.
        let pod = json!({"spec": {"containers": [], "volumes": [{"name": "v1", "hostPath": {"path": "/etc"}}]}});
        let violations = validate(&pod, Level::Restricted);
        let matching: Vec<_> = violations.iter().filter(|v| v.contains("v1")).collect();
        assert_eq!(matching.len(), 1, "hostPath must only be reported once, by restrictedVolumes, not also by hostPathVolumes: {violations:?}");
    }

    #[test]
    fn restricted_exempts_a_windows_pod_from_linux_only_checks() {
        let pod = json!({"spec": {"os": {"name": "windows"}, "containers": [{"name": "c1"}]}});
        let violations = validate(&pod, Level::Restricted);
        assert!(!violations
            .iter()
            .any(|v| v.contains("allowPrivilegeEscalation")
                || v.contains("capabilities")
                || v.contains("seccompProfile")));
    }

    #[test]
    fn a_fully_compliant_pod_passes_restricted() {
        let pod = json!({"spec": {
            "securityContext": {"runAsNonRoot": true, "seccompProfile": {"type": "RuntimeDefault"}},
            "containers": [{
                "name": "c1",
                "securityContext": {"allowPrivilegeEscalation": false, "capabilities": {"drop": ["ALL"]}},
            }],
        }});
        assert!(validate(&pod, Level::Restricted).is_empty());
    }

    #[test]
    fn applies_to_pod_create_only() {
        use crate::admission::attributes::Operation;
        assert!(applies_to("", "pods", "", Operation::Create));
        assert!(!applies_to("", "pods", "", Operation::Update));
        assert!(!applies_to("", "pods", "status", Operation::Create));
        assert!(!applies_to("apps", "pods", "", Operation::Create));
    }
}
