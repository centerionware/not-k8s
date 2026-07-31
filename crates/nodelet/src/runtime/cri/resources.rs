use super::*;

/// kubelet's fixed CPU CFS period (`--cpu-cfs-quota-period`'s default, 100ms
/// in microseconds) — quota is computed against this, not configurable here.
const CPU_CFS_QUOTA_PERIOD_US: i64 = 100_000;

/// Every non-cpu/memory resource in `limits`, as `(name, count)` — a pure
/// extraction so "does this container ask for an extended resource" is
/// unit-testable without a live `DevicePlugins` registry. Whether nodelet
/// actually has a driver for a given name (and so whether it's really a
/// device-plugin resource, as opposed to something with no kubelet-side
/// meaning at all) is decided by the caller via
/// `DevicePlugins::resource_configured()`.
pub(crate) fn extended_resource_requests(limits: Option<&BTreeMap<String, Quantity>>) -> Vec<(String, u64)> {
    let Some(limits) = limits else { return Vec::new() };
    limits
        .iter()
        .filter(|(name, _)| name.as_str() != "cpu" && name.as_str() != "memory")
        .filter_map(|(name, q)| parse_quantity(&q.0).map(|v| (name.clone(), v.round().max(0.0) as u64)))
        .collect()
}


/// A cpu Quantity as millicores: `"500m"` -> 500, `"2"` -> 2000, `"0.5"` -> 500.
pub(crate) fn parse_cpu_millicores(q: &Quantity) -> Option<i64> {
    let s = q.0.trim();
    if let Some(m) = s.strip_suffix('m') {
        return m.parse::<f64>().ok().map(|v| v.round() as i64);
    }
    parse_quantity(s).map(|cores| (cores * 1000.0).round() as i64)
}


/// A memory Quantity as bytes.
pub(crate) fn parse_memory_bytes(q: &Quantity) -> Option<i64> {
    parse_quantity(&q.0).map(|b| b.round() as i64)
}


/// kubelet's cpu.shares formula: `max(2, milliCPU * 1024 / 1000)`. No
/// request/limit at all still gets the cgroup-default minimum (2), same as
/// a real BestEffort pod.
pub(crate) fn cpu_shares_for(cpu_millicores: Option<i64>) -> i64 {
    match cpu_millicores {
        Some(m) if m > 0 => ((m * 1024) / 1000).max(2),
        _ => 2,
    }
}


/// What `ensure_container()` should do about an already-running container
/// whose live resources no longer match its (possibly just-edited) pod
/// spec — the in-place pod vertical scaling decision (round 42; found in
/// round 39's re-audit). Pulled out pure, same reasoning as every other
/// `*_decision()` function in this file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ResizeDecision {
    /// Live resources already match the pod spec — nothing to do.
    NoChange,
    /// A changed resource's `resizePolicy` allows applying it without a
    /// restart (or none was specified — `NotRequired` is the real default).
    UpdateInPlace,
    /// A changed resource's `resizePolicy` explicitly requires
    /// `RestartContainer` — the caller should recreate the container
    /// exactly like `RestartDecision::NeedsRestart` already does.
    RequiresRestart,
}


/// Compare the pod spec's *desired* resources (freshly computed via
/// `linux_resources()`) against the *actual* resources last recorded for
/// this container (`CriRuntime::container_resources`, already tracked for
/// CPU Manager's shared-pool refresh — round 16). Deliberately only
/// compares the pod-spec-derived fields (`cpu_shares`/`cpu_quota`/
/// `cpu_period`/`memory_limit_in_bytes`), never `cpuset_cpus`/`cpuset_mems`
/// — those are owned independently by CPU/Memory Manager and can change
/// for reasons that have nothing to do with a spec edit (a neighboring
/// container's exclusive claim coming or going), which must never itself
/// be mistaken for a resize request.
pub(crate) fn resize_decision(
    desired: &LinuxContainerResources,
    actual: &LinuxContainerResources,
    resize_policies: Option<&[ContainerResizePolicy]>,
) -> ResizeDecision {
    let cpu_changed =
        desired.cpu_shares != actual.cpu_shares || desired.cpu_quota != actual.cpu_quota || desired.cpu_period != actual.cpu_period;
    let memory_changed = desired.memory_limit_in_bytes != actual.memory_limit_in_bytes;
    if !cpu_changed && !memory_changed {
        return ResizeDecision::NoChange;
    }
    let restart_required_for = |resource_name: &str| -> bool {
        resize_policies
            .unwrap_or(&[])
            .iter()
            .find(|p| p.resource_name == resource_name)
            .map(|p| p.restart_policy == "RestartContainer")
            .unwrap_or(false) // unspecified defaults to NotRequired, matching the API's own documented default
    };
    if (cpu_changed && restart_required_for("cpu")) || (memory_changed && restart_required_for("memory")) {
        ResizeDecision::RequiresRestart
    } else {
        ResizeDecision::UpdateInPlace
    }
}


/// Translate a container's `resources` into CRI's `LinuxContainerResources`.
/// CPU shares come from requests (falling back to limits if there's no
/// request, matching kubelet); CPU quota/period and the memory limit come
/// from limits only — a limit-less resource is left at CRI's "unspecified"
/// zero value, which containerd/runc treat as unconstrained.
/// `qos`/`node_memory_bytes` (round 28) only feed `oom_score_adj` — see
/// `eviction::oom_score_adj()`. Real kubelet computes this per container
/// (not per pod), using each container's own memory *request*, which is
/// why it's threaded through here rather than computed once per pod.
pub(crate) fn linux_resources(resources: Option<&ResourceRequirements>, qos: QosClass, node_memory_bytes: i64) -> LinuxContainerResources {
    let requests = resources.and_then(|r| r.requests.as_ref());
    let limits = resources.and_then(|r| r.limits.as_ref());
    let cpu_request = requests.and_then(|m| m.get("cpu")).and_then(parse_cpu_millicores);
    let cpu_limit = limits.and_then(|m| m.get("cpu")).and_then(parse_cpu_millicores);
    let mem_limit = limits.and_then(|m| m.get("memory")).and_then(parse_memory_bytes);
    let mem_request = requests.and_then(|m| m.get("memory")).and_then(parse_memory_bytes).unwrap_or(0);

    let (cpu_quota, cpu_period) = match cpu_limit {
        Some(m) if m > 0 => (CPU_CFS_QUOTA_PERIOD_US * m / 1000, CPU_CFS_QUOTA_PERIOD_US),
        _ => (0, 0),
    };

    LinuxContainerResources {
        cpu_shares: cpu_shares_for(cpu_request.or(cpu_limit)),
        cpu_quota,
        cpu_period,
        memory_limit_in_bytes: mem_limit.unwrap_or(0),
        oom_score_adj: crate::eviction::oom_score_adj(qos, mem_request, node_memory_bytes),
        hugepage_limits: hugepage_limits(limits),
        ..Default::default()
    }
}


/// A k8s hugepage resource name's binary-unit suffix (`"Mi"`/`"Gi"`/`"Ki"`)
/// -> CRI's own `HugepageLimit.page_size` format (round 59; found in
/// round 58's re-audit) — `"<size><unit-prefix>B"` (e.g. `"2MB"`,
/// `"1GB"`), matching the corresponding `hugetlb.<pagesize>.limit_in_bytes`
/// cgroup file name exactly. Despite looking decimal, the proto's own doc
/// comment confirms these are still parsed base-1024 — this is purely a
/// naming-convention translation (drop the trailing `i`, append `B`), not
/// a unit conversion; the byte *value* itself needs no rescaling.
pub(crate) fn hugepage_cri_page_size(k8s_suffix: &str) -> Option<String> {
    k8s_suffix.strip_suffix('i').map(|s| format!("{s}B"))
}


/// Every `resources.limits["hugepages-<size>"]` entry -> CRI's
/// `HugepageLimit` list, which has direct native support for exactly this
/// (`LinuxContainerResources.hugepage_limits`) — no host-side mount or
/// separate mechanism needed, unlike `emptyDir`'s own (still separately
/// tracked, still open) HugePages volume medium.
pub(crate) fn hugepage_limits(limits: Option<&BTreeMap<String, Quantity>>) -> Vec<v1::HugepageLimit> {
    let Some(limits) = limits else { return Vec::new() };
    limits
        .iter()
        .filter_map(|(name, q)| {
            let suffix = name.strip_prefix("hugepages-")?;
            let page_size = hugepage_cri_page_size(suffix)?;
            let bytes = parse_memory_bytes(q)?;
            Some(v1::HugepageLimit { page_size, limit: bytes.max(0) as u64 })
        })
        .collect()
}


/// Translate `spec.overhead` (a flat `ResourceList`, not a request/limit
/// pair) into `LinuxContainerResources` for `LinuxPodSandboxConfig.overhead`
/// — the per-sandbox cost a `RuntimeClass` declares on top of its
/// containers' own resources (e.g. gVisor's userspace kernel). Treated the
/// same as a limit for CPU-quota/memory-limit purposes, since overhead is
/// an amount to reserve/cap against, not something with its own
/// request/limit distinction.
pub(crate) fn resource_list_to_linux_resources(list: &BTreeMap<String, Quantity>) -> LinuxContainerResources {
    let cpu_millicores = list.get("cpu").and_then(parse_cpu_millicores);
    let mem_bytes = list.get("memory").and_then(parse_memory_bytes);

    let (cpu_quota, cpu_period) = match cpu_millicores {
        Some(m) if m > 0 => (CPU_CFS_QUOTA_PERIOD_US * m / 1000, CPU_CFS_QUOTA_PERIOD_US),
        _ => (0, 0),
    };

    LinuxContainerResources {
        cpu_shares: cpu_shares_for(cpu_millicores),
        cpu_quota,
        cpu_period,
        memory_limit_in_bytes: mem_bytes.unwrap_or(0),
        ..Default::default()
    }
}


/// `securityContext.supplementalGroupsPolicy` (round 62; GA in k8s 1.33,
/// found in round 58's re-audit) -> CRI's own `SupplementalGroupsPolicy`
/// enum, which has direct native support for exactly this (both
/// `LinuxSandboxSecurityContext.supplemental_groups_policy` and
/// `LinuxContainerSecurityContext`'s own field of the same name) — no
/// image inspection or `/etc/group` parsing needed on nodelet's side at
/// all, the runtime does that. `None`/unset or anything other than the
/// literal `"Strict"` maps to `Merge`, matching both the k8s API's own
/// documented default ("If not specified, Merge is used") and CRI's
/// proto doc comment for this exact field.
pub(crate) fn supplemental_groups_policy_cri(policy: Option<&str>) -> v1::SupplementalGroupsPolicy {
    if policy == Some("Strict") {
        v1::SupplementalGroupsPolicy::Strict
    } else {
        v1::SupplementalGroupsPolicy::Merge
    }
}


/// Translate pod- and container-level `securityContext` into CRI's
/// `LinuxContainerSecurityContext`. Container-level fields override pod-level
/// ones wherever Kubernetes defines both (matches real kubelet semantics).
/// Not translated yet (see docs/GAP_CLOSURE.md): AppArmor profile, SELinux
/// options, and runAsNonRoot *verification* against the image's actual user
/// (that needs image inspection, not just pass-through).
pub(crate) fn linux_security_context(
    pod_sc: Option<&PodSecurityContext>,
    container_sc: Option<&SecurityContext>,
    pid_mode: NamespaceMode,
) -> LinuxContainerSecurityContext {
    let run_as_user = container_sc
        .and_then(|s| s.run_as_user)
        .or_else(|| pod_sc.and_then(|s| s.run_as_user));
    let run_as_group = container_sc
        .and_then(|s| s.run_as_group)
        .or_else(|| pod_sc.and_then(|s| s.run_as_group));
    let privileged = container_sc.and_then(|s| s.privileged).unwrap_or(false);
    let readonly_rootfs = container_sc.and_then(|s| s.read_only_root_filesystem).unwrap_or(false);
    let no_new_privs = container_sc.and_then(|s| s.allow_privilege_escalation) == Some(false);
    let capabilities = container_sc.and_then(|s| s.capabilities.as_ref()).map(|c| Capability {
        add_capabilities: c.add.clone().unwrap_or_default(),
        drop_capabilities: c.drop.clone().unwrap_or_default(),
        ..Default::default()
    });
    let supplemental_groups = pod_sc
        .and_then(|s| s.supplemental_groups.clone())
        .unwrap_or_default();
    let seccomp = seccomp_profile(pod_sc, container_sc);

    LinuxContainerSecurityContext {
        run_as_user: run_as_user.map(|value| Int64Value { value }),
        run_as_group: run_as_group.map(|value| Int64Value { value }),
        privileged,
        readonly_rootfs,
        no_new_privs,
        capabilities,
        supplemental_groups,
        supplemental_groups_policy: supplemental_groups_policy_cri(pod_sc.and_then(|s| s.supplemental_groups_policy.as_deref())) as i32,
        seccomp,
        namespace_options: Some(NamespaceOption { pid: pid_mode as i32, ..Default::default() }),
        ..Default::default()
    }
}


/// Container-level `seccompProfile` wins over the pod-level one, matching
/// Kubernetes' own override rule. `None` (neither set) means "let the
/// runtime pick its own default" — leaving CRI's `seccomp` field unset,
/// same as before this existed.
pub(crate) fn seccomp_profile(
    pod_sc: Option<&PodSecurityContext>,
    container_sc: Option<&SecurityContext>,
) -> Option<SecurityProfile> {
    let profile = container_sc
        .and_then(|s| s.seccomp_profile.as_ref())
        .or_else(|| pod_sc.and_then(|s| s.seccomp_profile.as_ref()))?;
    Some(match profile.type_.as_str() {
        "RuntimeDefault" => SecurityProfile { profile_type: ProfileType::RuntimeDefault as i32, ..Default::default() },
        "Localhost" => SecurityProfile {
            profile_type: ProfileType::Localhost as i32,
            localhost_ref: profile.localhost_profile.clone().unwrap_or_default(),
        },
        _ => SecurityProfile { profile_type: ProfileType::Unconfined as i32, ..Default::default() },
    })
}


