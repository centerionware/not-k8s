use super::*;

/// Identity extracted from a Pod object.
pub(crate) struct PodId {
    pub(crate) namespace: String,
    pub(crate) name: String,
    pub(crate) uid: String,
    pub(crate) host_network: bool,
    /// `spec.hostUsers` — `true` (the default, matching upstream) means no
    /// user namespace at all; only an explicit `false` triggers an
    /// exclusive UID/GID range allocation (see `userns.rs`, round 25).
    pub(crate) host_users: bool,
    /// `spec.hostPID`/`spec.hostIPC` (round 40) — share the host's PID/IPC
    /// namespace instead of an isolated one.
    pub(crate) host_pid: bool,
    pub(crate) host_ipc: bool,
    /// `spec.shareProcessNamespace` (round 40) — every container in the pod
    /// shares one PID namespace instead of each getting its own. Ignored
    /// (moot) when `host_pid` is also true.
    pub(crate) share_process_namespace: bool,
    /// `spec.serviceAccountName` (defaulted to `"default"` same as
    /// `pod_service_account_name()`), round 71 — needed to mint a
    /// `tokenAttributes`-scoped token for a credential provider without
    /// threading the whole `Pod` object through the image-pull call
    /// chain, same reasoning every other `PodId` field already exists for.
    pub(crate) service_account_name: String,
}


pub(crate) fn pod_id(pod: &Pod) -> PodId {
    let namespace = pod.metadata.namespace.clone().unwrap_or_else(|| "default".to_string());
    let name = pod.metadata.name.clone().unwrap_or_default();
    let uid = pod
        .metadata
        .uid
        .clone()
        .unwrap_or_else(|| format!("{namespace}_{name}"));
    let spec = pod.spec.as_ref();
    let host_network = spec.and_then(|s| s.host_network).unwrap_or(false);
    let host_users = spec.and_then(|s| s.host_users).unwrap_or(true);
    let host_pid = spec.and_then(|s| s.host_pid).unwrap_or(false);
    let host_ipc = spec.and_then(|s| s.host_ipc).unwrap_or(false);
    let share_process_namespace = spec.and_then(|s| s.share_process_namespace).unwrap_or(false);
    let service_account_name = pod_service_account_name(pod);
    PodId {
        namespace,
        name,
        uid,
        host_network,
        host_users,
        host_pid,
        host_ipc,
        share_process_namespace,
        service_account_name,
    }
}


/// Real kubelet's PID-namespace mode: `hostPID` wins outright (share the
/// host's), then `shareProcessNamespace` (every container in the pod shares
/// one), otherwise each container gets its own — CRI's proto default (unset
/// = `POD`) is the *opposite* of this and was round 40's actual correctness
/// finding, not just a missing feature (see `docs/GAP_CLOSURE.md` round 39).
pub(crate) fn pid_namespace_mode(host_pid: bool, share_process_namespace: bool) -> NamespaceMode {
    if host_pid {
        NamespaceMode::Node
    } else if share_process_namespace {
        NamespaceMode::Pod
    } else {
        NamespaceMode::Container
    }
}


/// `EphemeralContainer` has the same shape as `Container` minus a couple of
/// fields real kubelet itself doesn't honor for debug containers (`ports`,
/// notably — see the API doc comment on `EphemeralContainer.ports`) plus
/// `targetContainerName` (process-namespace-sharing target, not something
/// CRI's `ContainerConfig` has a slot for — nodelet always shares the
/// sandbox's containers via the sandbox's own PID namespace already, so this
/// is a no-op here rather than a gap).
pub(crate) fn ephemeral_to_container(ec: &EphemeralContainer) -> Container {
    Container {
        args: ec.args.clone(),
        command: ec.command.clone(),
        env: ec.env.clone(),
        env_from: ec.env_from.clone(),
        image: ec.image.clone(),
        image_pull_policy: ec.image_pull_policy.clone(),
        lifecycle: ec.lifecycle.clone(),
        liveness_probe: ec.liveness_probe.clone(),
        name: ec.name.clone(),
        ports: None,
        readiness_probe: ec.readiness_probe.clone(),
        resize_policy: ec.resize_policy.clone(),
        resources: ec.resources.clone(),
        restart_policy: ec.restart_policy.clone(),
        security_context: ec.security_context.clone(),
        startup_probe: ec.startup_probe.clone(),
        stdin: ec.stdin,
        stdin_once: ec.stdin_once,
        termination_message_path: ec.termination_message_path.clone(),
        termination_message_policy: ec.termination_message_policy.clone(),
        tty: ec.tty,
        volume_devices: ec.volume_devices.clone(),
        volume_mounts: ec.volume_mounts.clone(),
        working_dir: ec.working_dir.clone(),
    }
}


#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum ContainerKind {
    App,
    Init,
    Ephemeral,
}


pub(crate) fn container_labels(id: &PodId, container_name: &str, kind: ContainerKind) -> HashMap<String, String> {
    let mut l = sandbox_labels(id);
    l.insert(CTR_NAME_LABEL.to_string(), container_name.to_string());
    match kind {
        ContainerKind::App => {}
        ContainerKind::Init => {
            l.insert(CTR_INIT_LABEL.to_string(), "true".to_string());
        }
        ContainerKind::Ephemeral => {
            l.insert(CTR_EPHEMERAL_LABEL.to_string(), "true".to_string());
        }
    }
    l
}


/// `userns_mapping`, if `Some((host_id_base, length))`, means this pod's
/// sandbox already has an exclusive UID/GID range allocated for it
/// (`spec.hostUsers: false` — see `userns.rs`, round 25); the caller
/// (`run_sandbox()`) is responsible for the actual allocation since this
/// function stays pure/side-effect-free for testability. `None` (the
/// overwhelmingly common case, `hostUsers` unset or `true`) means no user
/// namespace at all — identical to this function's pre-round-25 behavior.
pub(crate) fn sandbox_config(
    id: &PodId,
    userns_mapping: Option<(u32, u32)>,
    hostname: &str,
    sysctls: &HashMap<String, String>,
    pod_sc: Option<&PodSecurityContext>,
) -> PodSandboxConfig {
    // Host-network pods set the network namespace to NODE, which makes the CRI
    // runtime skip CNI entirely (no pod network to set up). The `linux` block
    // is now always built (round 40) — CRI's own proto default for an unset
    // `pid` mode is `POD` (every container shares one PID namespace), the
    // *opposite* of real Kubernetes' actual default (each container gets its
    // own); always setting it explicitly is the fix, not an edge case.
    let userns_options = userns_mapping.map(|(host_id, length)| {
        let mapping = |container_id| IdMapping { host_id, container_id, length };
        UserNamespace { mode: NamespaceMode::Pod as i32, uids: vec![mapping(0)], gids: vec![mapping(0)] }
    });
    // IPC has no CONTAINER-scope concept in the Kubernetes API — containers
    // in a pod always share it unless `hostIPC` opts into sharing the host's.
    let ipc = if id.host_ipc { NamespaceMode::Node } else { NamespaceMode::Pod };
    let network = if id.host_network { NamespaceMode::Node } else { NamespaceMode::Pod };
    let linux = Some(LinuxPodSandboxConfig {
        security_context: Some(LinuxSandboxSecurityContext {
            namespace_options: Some(NamespaceOption {
                network: network as i32,
                pid: pid_namespace_mode(id.host_pid, id.share_process_namespace) as i32,
                ipc: ipc as i32,
                userns_options,
                ..Default::default()
            }),
            supplemental_groups_policy: supplemental_groups_policy_cri(
                pod_sc.and_then(|s| s.supplemental_groups_policy.as_deref()),
            ) as i32,
            ..Default::default()
        }),
        sysctls: sysctls.clone(),
        ..Default::default()
    });

    PodSandboxConfig {
        metadata: Some(PodSandboxMetadata {
            name: id.name.clone(),
            uid: id.uid.clone(),
            namespace: id.namespace.clone(),
            attempt: 0,
        }),
        // Host-network sandboxes share the host UTS namespace, so a hostname
        // cannot be set (runc rejects it). Real kubelets leave it empty too.
        hostname: if id.host_network { String::new() } else { hostname.to_string() },
        log_directory: format!("/var/log/pods/{}_{}_{}", id.namespace, id.name, id.uid),
        labels: sandbox_labels(id),
        linux,
        ..Default::default()
    }
}


impl CriRuntime {
    /// Look up our sandbox for a pod by namespace+name. These labels are always
    /// set (from real values), so this is the stable key — unlike `pod.uid`,
    /// which the agent does not have at status/teardown time.
    /// Returns the sandbox's id and CRI state (SANDBOX_READY / SANDBOX_NOTREADY
    /// as i32), not just existence — see ensure_pod()'s sandbox_reuse_decision()
    /// call for why the state matters: containerd's sandbox metadata can
    /// outlive its actual task/pause process (e.g. across a reboot — processes
    /// don't survive one, but the bolt-db record does), and reusing a
    /// not-ready sandbox as if it were live makes every CreateContainer
    /// against it fail forever with "no running task found".
    pub(crate) async fn find_sandbox(&self, namespace: &str, name: &str) -> Result<Option<(String, i32)>> {
        let mut rt = self.rt.clone();
        let filter = PodSandboxFilter {
            label_selector: HashMap::from([
                (POD_NS_LABEL.to_string(), namespace.to_string()),
                (POD_NAME_LABEL.to_string(), name.to_string()),
            ]),
            ..Default::default()
        };
        let resp = rt
            .list_pod_sandbox(ListPodSandboxRequest { filter: Some(filter) })
            .await?
            .into_inner();
        Ok(resp.items.into_iter().next().map(|s| (s.id, s.state)))
    }

    pub(crate) async fn run_sandbox(
        &self,
        id: &PodId,
        hostname: &str,
        sysctls: &HashMap<String, String>,
        dns: Option<DnsConfig>,
        runtime_handler: String,
        cgroup_parent: String,
        overhead: Option<LinuxContainerResources>,
        pod_sc: Option<&PodSecurityContext>,
    ) -> Result<String> {
        let mut rt = self.rt.clone();
        // spec.hostUsers: false (round 25) — allocate this pod an exclusive
        // host UID/GID range (keyed by pod uid, stable across reconciles/
        // retries; sandbox_id doesn't exist yet at this point). Allocation
        // failure (pool exhausted) falls back to no user namespace with a
        // warning rather than failing pod creation outright — the same
        // graceful-degradation posture CPU/Memory Manager already have.
        let userns_mapping = if !id.host_users {
            match self.userns.allocate(&id.uid) {
                Some(mapping) => Some(mapping),
                None => {
                    warn!(pod = %format!("{}/{}", id.namespace, id.name), "user namespace: no free UID/GID range available; falling back to the host user namespace");
                    None
                }
            }
        } else {
            None
        };
        let mut config = sandbox_config(id, userns_mapping, hostname, sysctls, pod_sc);
        config.dns_config = dns;
        let linux = config.linux.get_or_insert_with(LinuxPodSandboxConfig::default);
        linux.cgroup_parent = cgroup_parent;
        linux.overhead = overhead;
        let resp = rt
            .run_pod_sandbox(RunPodSandboxRequest { config: Some(config), runtime_handler })
            .await?
            .into_inner();
        Ok(resp.pod_sandbox_id)
    }

    /// Resolve `spec.runtimeClassName` to a CRI runtime handler name (e.g.
    /// `gvisor`, `kata`) via the cluster-scoped `RuntimeClass` object. Empty
    /// string (CRI's "use the default handler") if unset, the referenced
    /// RuntimeClass doesn't exist, or the lookup fails — a missing
    /// RuntimeClass should really block scheduling (real k8s validates this
    /// at admission), but nodelet doesn't implement that admission check,
    /// so falling back to the default runtime is safer than refusing to run
    /// the pod at all over a lookup it can't itself enforce.
    pub(crate) async fn resolve_runtime_handler(&self, pod: &Pod) -> String {
        let Some(class_name) = pod.spec.as_ref().and_then(|s| s.runtime_class_name.as_deref()) else {
            return String::new();
        };
        let api: Api<RuntimeClass> = Api::all(self.client.clone());
        match api.get(class_name).await {
            Ok(rc) => rc.handler,
            Err(e) => {
                warn!(runtime_class = %class_name, error = ?e, "failed to resolve RuntimeClass; using the default runtime handler");
                String::new()
            }
        }
    }

}
