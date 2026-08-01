use super::*;

/// kubelet's fixed CPU CFS period (`--cpu-cfs-quota-period`'s default, 100ms
/// in microseconds) — quota is computed against this, not configurable here.
const CPU_CFS_QUOTA_PERIOD_US: i64 = 100_000;

/// `container.lifecycle.stopSignal`'s k8s API spelling (e.g. `"SIGTERM"`,
/// `"SIGRTMIN+5"`) -> CRI's own `Signal` enum (round 66; GA 1.33, found
/// in a fresh gap re-audit — CRI's proto already had direct native
/// support for this, nobody had wired it up). k8s and CRI define exactly
/// the same 65-signal set (confirmed against upstream docs), just spelled
/// differently: CRI's generated enum strips the shared `SIGNAL_` prefix
/// and can't contain `+`/`-` in an identifier, so real-time signal
/// offsets become `PLUS`/`MINUS` words instead of the literal symbol.
/// Translating is therefore purely a naming-convention exercise (same
/// shape as round 59's `hugepage_cri_page_size()`), not a lookup table:
/// re-derive the proto's own constant name and let `Signal::from_str_name()`
/// (prost-generated) do the matching. Unrecognized input (shouldn't reach
/// here at all — apiserver validation restricts this field to exactly
/// the 65 real values) returns `None` rather than guessing.
pub(crate) fn stop_signal_cri(k8s_signal: &str) -> Option<i32> {
    let normalized = k8s_signal.replace('+', "PLUS").replace('-', "MINUS");
    v1::Signal::from_str_name(&format!("SIGNAL_{normalized}")).map(|s| s as i32)
}

/// `stop_signal_cri()`'s inverse — CRI's `Signal` enum value (as reported
/// back on `ContainerStatus.stop_signal`) -> k8s's own spelling, for
/// `containerStatuses[].stopSignal`. `RuntimeDefault` (CRI's zero value,
/// meaning "the runtime picked its own default, nothing explicit was
/// asked for") maps to `None` — real kubelet only reports a concrete
/// signal name here, never a sentinel for "unspecified."
pub(crate) fn stop_signal_k8s(cri_signal: i32) -> Option<String> {
    let signal = v1::Signal::try_from(cri_signal).ok()?;
    if signal == v1::Signal::RuntimeDefault {
        return None;
    }
    signal.as_str_name().strip_prefix("SIGNAL_").map(|s| s.replace("PLUS", "+").replace("MINUS", "-"))
}

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
    // memory_swap_limit_in_bytes (round 68) also changes on a
    // request-only edit under LimitedSwap (the memory limit itself can
    // stay put while the request-derived swap share moves) — comparing
    // it too catches that, which comparing memory_limit_in_bytes alone
    // wouldn't.
    let memory_changed =
        desired.memory_limit_in_bytes != actual.memory_limit_in_bytes || desired.memory_swap_limit_in_bytes != actual.memory_swap_limit_in_bytes;
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
pub(crate) fn linux_resources(
    resources: Option<&ResourceRequirements>,
    qos: QosClass,
    node_memory_bytes: i64,
    node_swap_bytes: i64,
    memory_swap_limited: bool,
) -> LinuxContainerResources {
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
        memory_swap_limit_in_bytes: container_swap_limit_bytes(
            mem_request,
            mem_limit.unwrap_or(0),
            node_memory_bytes,
            node_swap_bytes,
            memory_swap_limited,
        ),
        oom_score_adj: crate::eviction::oom_score_adj(qos, mem_request, node_memory_bytes),
        hugepage_limits: hugepage_limits(limits),
        ..Default::default()
    }
}

/// `memorySwap.swapBehavior` (round 68; GA 1.34, found in round 65's
/// fresh gap re-audit) -> CRI's `LinuxContainerResources.memory_swap_limit_in_bytes`,
/// which CRI's own proto/OCI runtime-spec both define as the *combined*
/// memory+swap ceiling (mirroring cgroup v1's `memory.memsw.limit_in_bytes`
/// naming even under cgroup v2, where the runtime derives the actual
/// `memory.swap.max` by subtracting `memory.max` internally) — this
/// function returns that combined value, never a swap-only one.
///
/// `NoSwap` (the default, matching upstream): every container with a
/// memory limit gets its swap ceiling pinned to exactly that limit (zero
/// *additional* swap) — a real correctness requirement, not just leaving
/// the field unset, since an unset field wouldn't override a node that
/// already has swap enabled at the OS level. A container with no memory
/// limit at all is left unconfigured (`0`, CRI's own "not specified"
/// sentinel) — there's no bound to peg a combined memory+swap value to.
///
/// `LimitedSwap`: implements the upstream KEP-2400 formula exactly —
/// `swapLimit = (containerMemoryRequest / nodeTotalMemory) * nodeTotalSwap`,
/// with `ContainerMemoryProportion` (and so the swap share) defined as
/// zero whenever `request == limit` (a Guaranteed-shaped container) or
/// the container has no memory request at all (BestEffort-shaped) —
/// between those two rules, only genuinely Burstable-shaped containers
/// (a request set, and no limit or a limit above the request) ever get a
/// nonzero share, without needing to compute the pod's overall QoS class
/// at all. **Known scope limitation**: nodelet has no
/// `--system-reserved`/`--kube-reserved`-equivalent knob for swap, so
/// `node_swap_bytes` is the node's raw `SwapTotal` with nothing withheld
/// — same simplification already accepted for ephemeral-storage (round
/// 48) and hugepages (round 60) capacity reporting.
pub(crate) fn container_swap_limit_bytes(mem_request: i64, mem_limit: i64, node_memory_bytes: i64, node_swap_bytes: i64, memory_swap_limited: bool) -> i64 {
    let no_extra_swap = || if mem_limit > 0 { mem_limit } else { 0 };
    if !memory_swap_limited {
        return no_extra_swap();
    }
    let guaranteed_shaped = mem_limit > 0 && mem_limit == mem_request;
    let best_effort_shaped = mem_request <= 0;
    if guaranteed_shaped || best_effort_shaped || node_memory_bytes <= 0 {
        return no_extra_swap();
    }
    if mem_limit <= 0 {
        // Burstable-shaped but no memory limit at all — no bound to
        // combine a swap share with; see the doc comment above.
        return 0;
    }
    let proportion = mem_request as f64 / node_memory_bytes as f64;
    let swap_share = (proportion * node_swap_bytes as f64).round() as i64;
    mem_limit + swap_share.clamp(0, node_swap_bytes.max(0))
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
    let (masked_paths, readonly_paths) = proc_mount_paths(container_sc.and_then(|s| s.proc_mount.as_deref()));

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
        masked_paths,
        readonly_paths,
        ..Default::default()
    }
}


/// `securityContext.procMount` (round 78; found in round 76's re-audit)
/// -> CRI's `LinuxContainerSecurityContext.masked_paths`/`.readonly_paths`.
/// Real kubelet always sets both explicitly rather than ever leaving
/// them for the runtime's own default (`pkg/securitycontext/util.go`'s
/// `ConvertToRuntimeMaskedPaths`/`ConvertToRuntimeReadonlyPaths`, mirrored
/// here) — `Default`/unset gets the standard Docker/OCI-recommended
/// masking list (`DEFAULT_MASKED_PATHS`/`DEFAULT_READONLY_PATHS`),
/// `Unmasked` gets two genuinely empty lists (real emptiness, not "field
/// omitted" — proto3 can't distinguish those on the wire either way, but
/// sending them explicitly matches upstream's own intent and avoids
/// depending on whatever a given CRI runtime happens to default to when
/// the field is left unset).
pub(crate) const DEFAULT_MASKED_PATHS: &[&str] = &[
    "/proc/asound",
    "/proc/acpi",
    "/proc/interrupts",
    "/proc/kcore",
    "/proc/keys",
    "/proc/latency_stats",
    "/proc/timer_list",
    "/proc/timer_stats",
    "/proc/sched_debug",
    "/proc/scsi",
    "/sys/firmware",
    "/sys/devices/virtual/powercap",
];

pub(crate) const DEFAULT_READONLY_PATHS: &[&str] =
    &["/proc/bus", "/proc/fs", "/proc/irq", "/proc/sys", "/proc/sysrq-trigger"];

pub(crate) fn proc_mount_paths(proc_mount: Option<&str>) -> (Vec<String>, Vec<String>) {
    if proc_mount == Some("Unmasked") {
        (Vec::new(), Vec::new())
    } else {
        (DEFAULT_MASKED_PATHS.iter().map(|s| s.to_string()).collect(), DEFAULT_READONLY_PATHS.iter().map(|s| s.to_string()).collect())
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


