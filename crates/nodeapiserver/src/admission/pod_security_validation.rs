/// Every landed `baseline`-level check, run in real upstream's own file
/// order — collects every failing check's message rather than stopping
/// at the first (matching `PodValidateLimitFunc`'s own "aggregate every
/// violation" posture, and real PSA's own `AggregateCheckResults`).
/// `include_overridden` is `false` at the `Restricted` level: real
/// upstream's own `OverrideCheckIDs` suppresses `hostPathVolumes`/
/// `capabilities_baseline`/`seccompProfile_baseline` there in favor of
/// their strictly-stronger restricted-level equivalents, so both don't
/// separately report overlapping violations for the same root cause.
fn baseline_violations(pod: &Value, include_overridden: bool) -> Vec<String> {
    let mut violations = vec![
        check_privileged(pod),
        check_host_namespaces(pod),
        check_host_ports(pod),
    ];
    if include_overridden {
        violations.push(check_host_path_volumes(pod));
        violations.push(check_capabilities_baseline(pod));
        violations.push(check_seccomp_profile_baseline(pod));
    }
    violations.extend([
        check_sysctls(pod),
        check_proc_mount(pod),
        check_host_probes_and_host_lifecycle(pod),
        check_windows_host_process(pod),
        check_apparmor_profile(pod),
        check_selinux_options(pod),
    ]);
    violations.into_iter().flatten().collect()
}

fn restricted_violations(pod: &Value) -> Vec<String> {
    let mut violations = baseline_violations(pod, false);
    violations.extend(
        [
            check_run_as_non_root(pod),
            check_run_as_user(pod),
            check_allow_privilege_escalation(pod),
            check_capabilities_restricted(pod),
            check_seccomp_profile_restricted(pod),
            check_restricted_volumes(pod),
        ]
        .into_iter()
        .flatten(),
    );
    violations
}

/// `level` is [`enforcement_level`]'s own output for the pod's namespace.
pub fn validate(pod: &Value, level: Level) -> Vec<String> {
    match level {
        Level::Privileged => Vec::new(),
        Level::Baseline => baseline_violations(pod, true),
        Level::Restricted => restricted_violations(pod),
    }
}
