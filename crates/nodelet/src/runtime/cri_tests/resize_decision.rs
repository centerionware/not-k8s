//! resize_decision(): in-place pod vertical scaling (Round 42; found in
//! round 39's re-audit). Covers the actual resize-application logic, not
//! the CPU/Memory Manager cpuset-refresh path this deliberately ignores.
use super::*;

fn resources(cpu_shares: i64, cpu_quota: i64, cpu_period: i64, memory_limit_in_bytes: i64) -> LinuxContainerResources {
    LinuxContainerResources { cpu_shares, cpu_quota, cpu_period, memory_limit_in_bytes, ..Default::default() }
}

fn resources_with_cpuset(cpu_shares: i64, cpuset_cpus: &str) -> LinuxContainerResources {
    LinuxContainerResources { cpu_shares, cpuset_cpus: cpuset_cpus.to_string(), ..Default::default() }
}

fn policy(resource_name: &str, restart_policy: &str) -> ContainerResizePolicy {
    ContainerResizePolicy { resource_name: resource_name.to_string(), restart_policy: restart_policy.to_string() }
}

#[test]
fn identical_resources_are_no_change() {
    let r = resources(512, 50_000, 100_000, 268_435_456);
    assert_eq!(resize_decision(&r, &r, None), ResizeDecision::NoChange);
}

#[test]
fn changed_cpu_with_no_resize_policy_defaults_to_update_in_place() {
    let desired = resources(1024, 100_000, 100_000, 268_435_456);
    let actual = resources(512, 50_000, 100_000, 268_435_456);
    assert_eq!(resize_decision(&desired, &actual, None), ResizeDecision::UpdateInPlace);
}

#[test]
fn changed_memory_with_no_resize_policy_defaults_to_update_in_place() {
    let desired = resources(512, 50_000, 100_000, 536_870_912);
    let actual = resources(512, 50_000, 100_000, 268_435_456);
    assert_eq!(resize_decision(&desired, &actual, None), ResizeDecision::UpdateInPlace);
}

#[test]
fn changed_cpu_with_explicit_not_required_policy_is_update_in_place() {
    let desired = resources(1024, 100_000, 100_000, 268_435_456);
    let actual = resources(512, 50_000, 100_000, 268_435_456);
    let policies = vec![policy("cpu", "NotRequired")];
    assert_eq!(resize_decision(&desired, &actual, Some(&policies)), ResizeDecision::UpdateInPlace);
}

#[test]
fn changed_cpu_with_restart_container_policy_requires_restart() {
    let desired = resources(1024, 100_000, 100_000, 268_435_456);
    let actual = resources(512, 50_000, 100_000, 268_435_456);
    let policies = vec![policy("cpu", "RestartContainer")];
    assert_eq!(resize_decision(&desired, &actual, Some(&policies)), ResizeDecision::RequiresRestart);
}

#[test]
fn changed_memory_with_restart_container_policy_requires_restart() {
    let desired = resources(512, 50_000, 100_000, 536_870_912);
    let actual = resources(512, 50_000, 100_000, 268_435_456);
    let policies = vec![policy("memory", "RestartContainer")];
    assert_eq!(resize_decision(&desired, &actual, Some(&policies)), ResizeDecision::RequiresRestart);
}

#[test]
fn a_restart_container_policy_for_an_unchanged_resource_does_not_force_a_restart() {
    // Only memory changed; cpu's RestartContainer policy is irrelevant here.
    let desired = resources(512, 50_000, 100_000, 536_870_912);
    let actual = resources(512, 50_000, 100_000, 268_435_456);
    let policies = vec![policy("cpu", "RestartContainer")];
    assert_eq!(resize_decision(&desired, &actual, Some(&policies)), ResizeDecision::UpdateInPlace);
}

#[test]
fn cpuset_cpus_differing_alone_is_not_a_resize_at_all() {
    // CPU Manager/Memory Manager own cpuset_cpus/cpuset_mems independently
    // (round 16/18) — a change there must never be mistaken for a pod-spec
    // resize request.
    let desired = resources_with_cpuset(512, "0-3");
    let actual = resources_with_cpuset(512, "4-7");
    assert_eq!(resize_decision(&desired, &actual, None), ResizeDecision::NoChange);
}
