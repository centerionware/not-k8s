use super::*;

impl CriRuntime {
    // Restart-on-exit: without this, a container that crashes (any reason —
    // app bug, a bad Corefile, transient resource pressure) sits exited
    // forever, `already` matches by name alone regardless of state, and
    // ensure_container becomes a permanent no-op for it. build_status() then
    // sees "all containers exited" and reports the *Pod* as Succeeded — a
    // terminal phase Kubernetes' ReplicaSet controller treats as permanently
    // inactive (isPodActive excludes Succeeded/Failed), so it creates a
    // replacement. Forever, once per crash. Confirmed for real: this is
    // exactly what was driving unbounded coredns pod creation — coredns's
    // container was exiting seconds after starting, nodelet never restarted
    // it, and every single exit silently manufactured a brand new pod
    // instead of the crash-looping restart-in-place a real kubelet gives a
    // restartPolicy: Always pod (the default, and what every Deployment
    // uses). "Never" is left alone — matches the one-shot Job-style pods
    // that policy is for.
    pub(crate) async fn ensure_container(
        &self,
        sandbox_id: &str,
        id: &PodId,
        container: &Container,
        pod_sc: Option<&PodSecurityContext>,
        restart_policy: &str,
        volumes: &HashMap<String, ResolvedVolume>,
        pull_secrets: &[String],
        envs: &[KeyValue],
        qos: QosClass,
        claim_devices: &HashMap<String, PreparedPodClaim>,
        runtime_handler: &str,
        privileged: bool,
    ) -> Result<()> {
        // Resize status reporting (round 43): record what the pod spec is
        // currently asking for, every reconcile, regardless of whether a
        // resize below succeeds/fails/isn't needed — nodelet has no
        // admission/deferral layer, so "allocated" always just mirrors the
        // live spec. Reported as `containerStatuses[].allocatedResources`.
        self.spec_resources.lock().unwrap().insert(
            restart_count_key(sandbox_id, &container.name),
            container.resources.as_ref().and_then(|r| r.requests.clone()).unwrap_or_default(),
        );

        let running_v = ContainerState::ContainerRunning as i32;
        let existing = self.list_pod_containers(sandbox_id).await?;
        let existing_ctr = existing
            .iter()
            .find(|c| c.labels.get(CTR_NAME_LABEL).map(|n| n == &container.name).unwrap_or(false));

        let needs_restart;
        // Only a genuine "container exited on its own, restartPolicy
        // allows retrying it" restart is subject to crash-loop backoff
        // (round 73) -- an in-place-resize-triggered restart of an
        // otherwise-healthy running container is a completely different,
        // operator-driven event and must never be throttled by it.
        let mut is_crash_restart = false;
        match restart_decision(existing_ctr.map(|c| c.state), running_v, restart_policy) {
            RestartDecision::LeaveTerminated => return Ok(()),
            RestartDecision::AlreadyRunning => {
                // In-place pod vertical scaling (round 42; found in round 39's
                // re-audit): the container's live resources may no longer
                // match its (possibly just-edited) pod spec. Compare against
                // the resources actually last applied (`container_resources`,
                // already tracked for CPU Manager's shared-pool refresh —
                // round 16) rather than recomputing from scratch, so a
                // CPU/Memory Manager-driven cpuset change is never itself
                // mistaken for a resize request (`resize_decision()` only
                // looks at the pod-spec-derived fields).
                let key = restart_count_key(sandbox_id, &container.name);
                let recorded = self.container_resources.lock().unwrap().get(&key).cloned();
                if let Some((container_id, actual)) = recorded {
                    let desired = linux_resources(container.resources.as_ref(), qos, self.node_memory_bytes, self.node_swap_bytes, self.memory_swap_limited);
                    match resize_decision(&desired, &actual, container.resize_policy.as_deref()) {
                        ResizeDecision::NoChange => return Ok(()),
                        ResizeDecision::UpdateInPlace => {
                            let mut updated = actual;
                            updated.cpu_shares = desired.cpu_shares;
                            updated.cpu_quota = desired.cpu_quota;
                            updated.cpu_period = desired.cpu_period;
                            updated.memory_limit_in_bytes = desired.memory_limit_in_bytes;
                            updated.memory_swap_limit_in_bytes = desired.memory_swap_limit_in_bytes;
                            updated.oom_score_adj = desired.oom_score_adj;
                            let mut rt = self.rt.clone();
                            match rt
                                .update_container_resources(UpdateContainerResourcesRequest {
                                    container_id: container_id.clone(),
                                    linux: Some(updated.clone()),
                                    ..Default::default()
                                })
                                .await
                            {
                                Ok(_) => {
                                    self.container_resources.lock().unwrap().insert(key.clone(), (container_id, updated));
                                    self.applied_resources.lock().unwrap().insert(key, container.resources.clone().unwrap_or_default());
                                }
                                Err(e) => {
                                    warn!(container = %container.name, error = ?e, "in-place resize: UpdateContainerResources failed; leaving the container's resources unchanged for now");
                                }
                            }
                            return Ok(());
                        }
                        ResizeDecision::RequiresRestart => needs_restart = true,
                    }
                } else {
                    return Ok(()); // nothing recorded yet (shouldn't happen for a running container) — nothing to compare against
                }
            }
            RestartDecision::NeedsRestart => {
                needs_restart = true;
                is_crash_restart = existing_ctr.is_some();
            }
        }

        // Crash-loop backoff (round 73; found in round 72's re-audit): a
        // container that keeps exiting doesn't get recreated as fast as
        // this event-driven controller can react (every status write is
        // itself a Pod modification that re-triggers a watch event and
        // another reconcile -- without this gate that feedback loop has
        // no natural rate limit at all). The exited container is simply
        // left in place until the backoff window elapses; the next
        // trigger (another status-write echo, a future watch event, or
        // this same pod's own probe/eviction paths) re-evaluates it, same
        // "no periodic poll, just react to the next edge" posture the
        // rest of this controller already has.
        if is_crash_restart && !self.restart_backoff_ready(sandbox_id, &container.name) {
            return Ok(());
        }

        if needs_restart {
            // Not running (or a resize policy demanded a restart) and this
            // pod is allowed to restart — clear the stale container out (if
            // there was one) so the create-below gets a fresh one.
            // Best-effort: if it's already gone by the time we ask, or CRI
            // won't remove it for some other reason, fall through and let
            // CreateContainer surface any real problem instead of masking
            // it here.
            if let Some(c) = existing_ctr {
                self.bump_restart_count(sandbox_id, &container.name);
                if is_crash_restart {
                    self.record_restart_backoff(sandbox_id, &container.name);
                }
                // lastState (round 75): capture this instance's own
                // terminated details right before it's gone for good —
                // otherwise there's no way to report it once the fresh
                // instance below has replaced it. Only meaningful for an
                // instance that actually exited (not one being killed
                // mid-run for a resize); best-effort, same as everything
                // else in this teardown path.
                if c.state == ContainerState::ContainerExited as i32 {
                    if let Ok(details) = self.container_status_details(&c.id).await {
                        let reason = if !details.reason.is_empty() {
                            details.reason
                        } else if details.exit_code == 0 {
                            "Completed".to_string()
                        } else {
                            "Error".to_string()
                        };
                        let finished_at =
                            (details.finished_at > 0).then(|| Timestamp::from_nanosecond(details.finished_at as i128).ok()).flatten();
                        let message = read_termination_message(&termination_message_host_path(&id.uid, &container.name));
                        self.record_last_terminated(
                            sandbox_id,
                            &container.name,
                            crate::runtime::TerminatedInfo {
                                container_id: Some(format_container_id(&self.runtime_name, &c.id)),
                                exit_code: details.exit_code,
                                reason,
                                finished_at,
                                message,
                            },
                        );
                    }
                }
                self.release_container_devices(sandbox_id, &container.name).await;
                let mut rt = self.rt.clone();
                let _ = rt.remove_container(RemoveContainerRequest { container_id: c.id.clone() }).await;
            }
        }

        let attempt = self.restart_count(sandbox_id, &container.name);
        self.create_and_start_container(
            sandbox_id, id, container, pod_sc, volumes, pull_secrets, envs, ContainerKind::App, attempt, qos, claim_devices, runtime_handler, privileged,
        )
        .await
    }

    /// Ephemeral (debug) containers are one-shot: unlike app containers,
    /// once one exists (running or exited) it's never recreated or
    /// restarted, no matter the pod's `restartPolicy` — matches real
    /// kubelet, which doesn't support removing or re-running a debug
    /// container once added.
    pub(crate) async fn ensure_ephemeral_container(
        &self,
        sandbox_id: &str,
        id: &PodId,
        pod: &Pod,
        container: &Container,
        pod_sc: Option<&PodSecurityContext>,
        volumes: &HashMap<String, ResolvedVolume>,
        pull_secrets: &[String],
        service_env: &BTreeMap<String, Vec<u8>>,
        claim_devices: &HashMap<String, PreparedPodClaim>,
        runtime_handler: &str,
        privileged: bool,
    ) -> Result<()> {
        let existing = self.list_pod_containers(sandbox_id).await?;
        let already_exists = existing.iter().any(|c| {
            c.labels.get(CTR_NAME_LABEL).map(|n| n == &container.name).unwrap_or(false)
                && c.labels.get(CTR_EPHEMERAL_LABEL).map(|v| v == "true").unwrap_or(false)
        });
        if already_exists {
            return Ok(());
        }
        let envs = self.resolve_container_env(pod, id, container, service_env).await?;
        let qos = crate::eviction::qos_class(pod);
        self.create_and_start_container(
            sandbox_id, id, container, pod_sc, volumes, pull_secrets, &envs, ContainerKind::Ephemeral, 0, qos, claim_devices, runtime_handler, privileged,
        )
        .await
    }

    /// The actual pull+create+start, shared by app containers
    /// (`ensure_container`) and init containers (`ensure_init_containers`) —
    /// they differ only in *when* to call this and what to do with an
    /// already-existing container, not in how a fresh one gets built.
    pub(crate) async fn create_and_start_container(
        &self,
        sandbox_id: &str,
        id: &PodId,
        container: &Container,
        pod_sc: Option<&PodSecurityContext>,
        volumes: &HashMap<String, ResolvedVolume>,
        pull_secrets: &[String],
        envs: &[KeyValue],
        kind: ContainerKind,
        attempt: u32,
        qos: QosClass,
        claim_devices: &HashMap<String, PreparedPodClaim>,
        runtime_handler: &str,
        privileged: bool,
    ) -> Result<()> {
        let image = container.image.clone().unwrap_or_default();
        let auth = self.resolve_pull_auth(id, pull_secrets, &image).await;
        let image_spec = ImageSpec { image: image.clone(), ..Default::default() };

        // imagePullPolicy (round 51; found in round 50's re-audit): `Always`
        // still pulls unconditionally (containerd itself no-ops if the
        // digest is already current) — the point of `IfNotPresent`/`Never`
        // is avoiding the *network round-trip* to the registry entirely,
        // which matters on a genuinely offline edge device even when the
        // image is already cached locally.
        let policy = effective_pull_policy(container.image_pull_policy.as_deref(), &image);
        let mut img = self.img.clone();
        let already_present = if policy != "Always" {
            img.image_status(ImageStatusRequest { image: Some(image_spec.clone()), verbose: false })
                .await
                .ok()
                .and_then(|r| r.into_inner().image)
                .is_some()
        } else {
            false
        };
        let need_pull = match policy {
            "Always" => true,
            "Never" => {
                if !already_present {
                    anyhow::bail!("imagePullPolicy: Never, but image '{image}' is not present on this node");
                }
                false
            }
            _ => !already_present, // IfNotPresent
        };
        if need_pull {
            img.pull_image(PullImageRequest {
                image: Some(image_spec.clone()),
                auth,
                sandbox_config: Some(sandbox_config(id, None, &id.name, &HashMap::new(), None, false)),
            })
            .await
            .context("pulling image")?;
        }

        // Round 88: this pod's own userns range, if it has one — the same
        // one run_sandbox() already allocated and applied at the sandbox
        // level (round 25); read-only here, never allocates. Round 123: no
        // longer threaded into build_mounts() as a per-mount idmap — see
        // volumes_pure.rs's removed mount_id_mappings() doc comment for
        // why that was actively wrong. Still needed here for the aux
        // mounts' own chown_userns_base() calls below.
        let userns_mapping = (!id.host_users).then(|| self.userns.assigned(&id.uid)).flatten();
        let handler_supports_recursive_ro = self.handler_supports_recursive_ro(runtime_handler);
        let mut mounts = build_mounts(container.volume_mounts.as_deref().unwrap_or(&[]), volumes, envs, handler_supports_recursive_ro);
        // `containerStatuses[].volumeMounts` (round 91; found in round 89's
        // re-audit): entirely derived from the spec, no RPC needed — cached
        // here (same key shape as `container_users`) for `build_status()`
        // to read back via plain lookup.
        self.container_volume_mount_statuses.lock().unwrap().insert(
            restart_count_key(sandbox_id, &container.name),
            volume_mount_status_tuples(container.volume_mounts.as_deref().unwrap_or(&[]), handler_supports_recursive_ro),
        );
        // Round 98 (found in round 88's own documented follow-up): this
        // pod's userns range applies to nodelet's own auxiliary
        // host-bind-mounts too, not just regular volumeMounts -- round 123
        // switched this (like every other mount) from a per-mount idmap to
        // just chowning the host-side file/dir to host_base, so the
        // sandbox's own ambient namespace translates it correctly.
        if let Some(ResolvedVolume::HostPath(hosts_path)) = volumes.get(ETC_HOSTS_VOLUME_KEY) {
            if let Some((host_base, _length)) = userns_mapping {
                if let Err(e) = chown_userns_base(hosts_path, host_base) {
                    warn!(path = %hosts_path.display(), host_base, error = ?e, "failed to chown /etc/hosts to the pod's userns base uid/gid");
                }
            }
            mounts.push(Mount {
                container_path: "/etc/hosts".to_string(),
                host_path: hosts_path.to_string_lossy().into_owned(),
                readonly: false,
                ..Default::default()
            });
        }
        // `terminationMessagePath` (round 24): bind-mount an empty host file
        // in at the container's requested path (default `/dev/termination-log`,
        // matching the apiserver's own defaulting) so nodelet can read
        // whatever the container writes there back out after it exits — the
        // same host-file-bind-mount approach real kubelet uses, not a CRI
        // concept at all. App and init containers only; ephemeral/debug
        // containers keep round 8's existing "exit codes not tracked"
        // simplification (see `build_labeled_container_statuses()`).
        if matches!(kind, ContainerKind::App | ContainerKind::Init) {
            let host_path = termination_message_host_path(&id.uid, &container.name);
            if let Some(parent) = host_path.parent() {
                std::fs::create_dir_all(parent).context("creating termination-message host directory")?;
            }
            if !host_path.exists() {
                std::fs::File::create(&host_path).context("creating termination-message host file")?;
            }
            // Round 123 (found live in CI): same real ownership requirement
            // as `resolve_volumes()`'s own userns chown loop and the
            // /etc/hosts mount just above — left at nodelet's own default
            // (host root, since nodelet creates this file running as host
            // root), it's outside the sandbox's mapped range and the
            // container's own writes to /dev/termination-log would hit the
            // same silent EACCES the etc-hosts/emptyDir case did.
            if let Some((host_base, _length)) = userns_mapping {
                if let Err(e) = chown_userns_base(&host_path, host_base) {
                    warn!(path = %host_path.display(), host_base, error = ?e, "failed to chown termination-message file to the pod's userns base uid/gid");
                }
            }
            let container_path =
                container.termination_message_path.clone().filter(|p| !p.is_empty()).unwrap_or_else(|| "/dev/termination-log".to_string());
            mounts.push(Mount {
                container_path,
                host_path: host_path.to_string_lossy().into_owned(),
                readonly: false,
                ..Default::default()
            });
        }
        let mut resources = linux_resources(container.resources.as_ref(), qos, self.node_memory_bytes, self.node_swap_bytes, self.memory_swap_limited);
        let limits = container.resources.as_ref().and_then(|r| r.limits.as_ref());
        let cpu_limit = limits.and_then(|m| m.get("cpu")).and_then(parse_cpu_millicores);
        let mem_limit = limits.and_then(|m| m.get("memory")).and_then(parse_memory_bytes);
        let wants_exclusive_cpus = crate::cpu_manager::wants_exclusive_cpus(qos, cpu_limit);
        let wants_pinned_memory = crate::memory_manager::wants_pinned_memory(qos, mem_limit);
        let device_requests: Vec<(String, u64)> =
            extended_resource_requests(limits).into_iter().filter(|(name, _)| self.device_plugins.resource_configured(name)).collect();

        // Topology Manager (opt-in — see topology.rs): find a single NUMA
        // node that can satisfy this container's exclusive-CPU want (if
        // any), pinned-memory want (if any), and every device-plugin
        // resource it needs (if any), so they don't end up scattered
        // across nodes. A no-op (nothing preferred, exactly pre-round-17
        // behavior) when the policy is `none`, or when this container has
        // nothing for it to coordinate at all. `Restricted` (round 20)
        // falls back to `topology::spread()` — each provider placed on its
        // own best node independently — when no single node works for
        // everyone; `SingleNumaNode` never does (see topology.rs).
        enum HintKind {
            Cpu,
            Memory,
            Device(String),
        }
        let mut cpu_preferred_node: Option<u32> = None;
        let mut memory_preferred_node: Option<u32> = None;
        let mut device_preferred_nodes: HashMap<String, u32> = HashMap::new();
        if self.topology_policy != crate::topology::TopologyManagerPolicy::None {
            let mut hints = Vec::new();
            let mut hint_kinds = Vec::new();
            if let (Some(count), Some(cpu_manager)) = (wants_exclusive_cpus, &self.cpu_manager) {
                hints.push(crate::topology::cpu_hint(&self.numa_topology, &cpu_manager.shared_pool(), count));
                hint_kinds.push(HintKind::Cpu);
            }
            if let (Some(bytes), Some(memory_manager)) = (wants_pinned_memory, &self.memory_manager) {
                hints.push(crate::topology::memory_hint(&memory_manager.free_per_node(), bytes));
                hint_kinds.push(HintKind::Memory);
            }
            for (resource_name, count) in &device_requests {
                let available = self.device_plugins.available_device_numa_nodes(resource_name);
                let all_nodes: std::collections::BTreeSet<u32> = self.numa_topology.keys().copied().collect();
                hints.push(crate::topology::device_hint(&available, &all_nodes, *count as u32));
                hint_kinds.push(HintKind::Device(resource_name.clone()));
            }
            if !hints.is_empty() {
                let apply = |node: u32, kind: &HintKind, cpu: &mut Option<u32>, mem: &mut Option<u32>, dev: &mut HashMap<String, u32>| match kind {
                    HintKind::Cpu => *cpu = Some(node),
                    HintKind::Memory => *mem = Some(node),
                    HintKind::Device(name) => {
                        dev.insert(name.clone(), node);
                    }
                };
                match crate::topology::align(&hints) {
                    Some(node) => {
                        for kind in &hint_kinds {
                            apply(node, kind, &mut cpu_preferred_node, &mut memory_preferred_node, &mut device_preferred_nodes);
                        }
                    }
                    None => match self.topology_policy {
                        crate::topology::TopologyManagerPolicy::SingleNumaNode => {
                            anyhow::bail!(
                                "Topology Manager: no single NUMA node can satisfy container '{}'s CPU/memory/device requests together",
                                container.name
                            );
                        }
                        crate::topology::TopologyManagerPolicy::Restricted => match crate::topology::spread(&hints) {
                            Some(nodes) => {
                                for (kind, node) in hint_kinds.iter().zip(nodes) {
                                    apply(node, kind, &mut cpu_preferred_node, &mut memory_preferred_node, &mut device_preferred_nodes);
                                }
                                warn!(container = %container.name, "Topology Manager: no single NUMA node satisfies every request together; spreading each across its own best node (restricted policy)");
                            }
                            None => {
                                anyhow::bail!(
                                    "Topology Manager: some request in container '{}' can't be satisfied on any NUMA node at all",
                                    container.name
                                );
                            }
                        },
                        crate::topology::TopologyManagerPolicy::BestEffort => {
                            warn!(container = %container.name, "Topology Manager: no aligned NUMA node found; proceeding without alignment (best-effort policy)");
                        }
                        crate::topology::TopologyManagerPolicy::None => unreachable!("guarded above"),
                    },
                }
            }
        }
        let preferred_cpus = cpu_preferred_node.and_then(|node| self.numa_topology.get(&node));

        // CPU Manager (static policy, opt-in — see cpu_manager.rs): a
        // Guaranteed-QoS container requesting a whole number of CPUs gets
        // pinned to exclusive cores (preferring the Topology Manager's
        // aligned NUMA node, if any); every other container gets the
        // current shared pool (everything except reserved + exclusively-
        // claimed cores) instead of being left unconstrained. Both are
        // no-ops when the policy is disabled (self.cpu_manager is None).
        let mut cpu_manager_key: Option<String> = None;
        if let Some(cpu_manager) = &self.cpu_manager {
            let key = restart_count_key(sandbox_id, &container.name);
            let cpuset = match wants_exclusive_cpus {
                Some(count) => match cpu_manager.allocate_preferring(&key, count, preferred_cpus) {
                    Some(cpus) => {
                        cpu_manager_key = Some(key);
                        cpus
                    }
                    None => {
                        warn!(container = %container.name, wanted = count, "CPU Manager: not enough exclusive CPUs available; falling back to the shared pool");
                        cpu_manager.shared_pool()
                    }
                },
                None => cpu_manager.shared_pool(),
            };
            resources.cpuset_cpus = crate::cpu_manager::format_cpuset(&cpuset);
        }

        // Memory Manager (static policy, opt-in — see memory_manager.rs):
        // a Guaranteed-QoS container with a memory limit set gets its
        // memory pinned to a single NUMA node (preferring the Topology
        // Manager's aligned node, if any). Unlike CPU Manager, non-pinned
        // containers are left with `cpuset_mems` unset ("unconstrained")
        // rather than tracked in a shared pool — see memory_manager.rs's
        // module doc comment for why. A no-op when the policy is disabled
        // (self.memory_manager is None).
        let mut memory_manager_key: Option<String> = None;
        if let (Some(bytes), Some(memory_manager)) = (wants_pinned_memory, &self.memory_manager) {
            let key = restart_count_key(sandbox_id, &container.name);
            match memory_manager.allocate_preferring(&key, bytes, memory_preferred_node) {
                Some(node) => {
                    memory_manager_key = Some(key);
                    resources.cpuset_mems = node.to_string();
                }
                None => {
                    warn!(container = %container.name, wanted = bytes, "Memory Manager: no single NUMA node has enough free capacity; leaving memory unpinned");
                }
            }
        }

        let resources_for_record = resources.clone();
        let linux = Some(LinuxContainerConfig {
            resources: Some(resources),
            security_context: Some(linux_security_context(
                pod_sc,
                container.security_context.as_ref(),
                pid_namespace_mode(id.host_pid, id.share_process_namespace),
                userns_mapping,
            )),
        });

        // Device plugin resources (nvidia.com/gpu and similar): allocate
        // specific devices (preferring the Topology Manager's aligned NUMA
        // node, if any) for each resource this container's limits name
        // that a registered device plugin actually backs, and merge in
        // whatever envs/mounts/device-nodes/annotations the plugin's
        // Allocate() RPC says to inject. Best-effort per resource — a
        // plugin failure means the container starts without that device
        // rather than failing the whole pod, logged clearly either way.
        let mut envs = envs.to_vec();
        // Raw block volumes (round 77; found in round 76's re-audit):
        // volumeDevices entries resolve straight to CRI device injection,
        // the same ContainerConfig.devices field device-plugin
        // allocations below also feed — no gating needed (a container
        // with none just gets an empty Vec).
        let mut devices = build_devices(container.volume_devices.as_deref().unwrap_or(&[]), volumes);
        let mut annotations = HashMap::new();
        let mut allocated_devices: Vec<(String, Vec<String>)> = Vec::new();
        for (resource_name, count) in device_requests {
            let preferred = device_preferred_nodes.get(&resource_name).copied();
            match self.device_plugins.allocate_preferring(&resource_name, count, preferred).await {
                Ok((device_ids, resp)) => {
                    envs.extend(resp.envs.into_iter().map(|(key, value)| KeyValue { key, value: value.into_bytes() }));
                    mounts.extend(resp.mounts.into_iter().map(|m| Mount {
                        container_path: m.container_path,
                        host_path: m.host_path,
                        readonly: m.read_only,
                        ..Default::default()
                    }));
                    devices.extend(resp.devices.into_iter().map(|d| v1::Device {
                        container_path: d.container_path,
                        host_path: d.host_path,
                        permissions: d.permissions,
                    }));
                    annotations.extend(resp.annotations);
                    allocated_devices.push((resource_name, device_ids));
                }
                Err(e) => {
                    warn!(container = %container.name, resource = %resource_name, error = ?e, "device plugin Allocate() failed; container will start without this device");
                }
            }
        }

        // Dynamic Resource Allocation (round 63): every
        // `resources.claims[].{name, request}` entry this container
        // declares -> the CDI device IDs `resolve_pod_claim_devices()`
        // (called once up-front in `ensure_pod()`) already prepared for
        // its pod-claim. No RPC here — that already happened; this is
        // purely the per-container lookup/attach step.
        let cdi_devices: Vec<v1::CdiDevice> = container
            .resources
            .as_ref()
            .and_then(|r| r.claims.as_ref())
            .into_iter()
            .flatten()
            .flat_map(|c| cdi_devices_for_container_claim(&c.name, c.request.as_deref(), claim_devices))
            .map(|name| v1::CdiDevice { name })
            .collect();

        // lifecycle.stopSignal (round 66; GA 1.33, found in a fresh gap
        // re-audit) — an unset field, or one this build's vendored proto
        // doesn't recognize, leaves CRI's own zero value (RuntimeDefault:
        // "the runtime picks its own default"), exactly matching the
        // unset case's real behavior.
        let stop_signal = container
            .lifecycle
            .as_ref()
            .and_then(|l| l.stop_signal.as_deref())
            .and_then(stop_signal_cri)
            .unwrap_or(v1::Signal::RuntimeDefault as i32);

        let mut rt = self.rt.clone();
        let config = ContainerConfig {
            metadata: Some(ContainerMetadata { name: container.name.clone(), attempt }),
            image: Some(image_spec),
            // `$(VAR)` expansion (found live testing a real CSI driver —
            // see `expand_command_arg()`'s doc comment): every command/args
            // entry can reference this container's own env vars, same as
            // `subPathExpr` already does for volume mounts.
            command: container.command.clone().unwrap_or_default().iter().map(|s| expand_command_arg(s, &envs)).collect(),
            args: container.args.clone().unwrap_or_default().iter().map(|s| expand_command_arg(s, &envs)).collect(),
            working_dir: container.working_dir.clone().unwrap_or_default(),
            envs,
            mounts,
            devices,
            annotations,
            labels: container_labels(id, &container.name, kind),
            log_path: format!("{}_{}.log", container.name, attempt),
            linux,
            cdi_devices,
            stop_signal,
            ..Default::default()
        };

        // `sandbox_config` here is CRI's own redundant "same config as
        // RunPodSandboxRequest, passed again just for reference" field (see
        // its own proto doc comment) — but it's what containerd's CRI
        // plugin actually reads back to decide whether a privileged
        // container may be created, *not* whatever was stored at
        // RunPodSandbox time. Found live: even with the sandbox itself
        // genuinely created with `privileged: true` (confirmed via
        // `crictl inspectp`'s `info.config`), every CreateContainer for a
        // real CSI driver's privileged container still failed with "no
        // privileged container allowed in sandbox" — because this call
        // site was rebuilding a *fresh* `sandbox_config()` with a
        // hardcoded `false`, unconditionally, regardless of the pod's
        // actual requirement. Must match what `run_sandbox()` actually
        // used, or this redundant copy silently overrides it. Round 123:
        // the exact same lesson applied to `userns_mapping` too, missed
        // the first time — this call site hardcoded `None` here even for
        // a `hostUsers: false` pod, so containerd's own consistency check
        // (`internal/cri/server/container_create.go`'s `sameUsernsConfig`,
        // confirmed by reading its real source) compared the container's
        // real userns_options against this redundant copy's `None` and
        // rejected every such container outright with "user namespace
        // config for sandbox is different from container" — nothing to do
        // with containerd's version at all; it was correctly enforcing a
        // real mismatch nodelet's own request introduced.
        let created = match rt
            .create_container(CreateContainerRequest {
                pod_sandbox_id: sandbox_id.to_string(),
                config: Some(config),
                sandbox_config: Some(sandbox_config(id, userns_mapping, &id.name, &HashMap::new(), None, privileged)),
            })
            .await
        {
            Ok(resp) => resp.into_inner(),
            Err(e) => {
                self.release_devices(&allocated_devices);
                if let (Some(cpu_manager), Some(key)) = (&self.cpu_manager, &cpu_manager_key) {
                    cpu_manager.release(key);
                }
                if let (Some(memory_manager), Some(key)) = (&self.memory_manager, &memory_manager_key) {
                    memory_manager.release(key);
                }
                return Err(e).context("creating container");
            }
        };
        self.record_device_allocations(sandbox_id, &container.name, allocated_devices);

        if let Err(e) = rt.start_container(StartContainerRequest { container_id: created.container_id.clone() }).await {
            self.release_container_devices(sandbox_id, &container.name).await;
            return Err(e).context("starting container");
        }

        // ContainerStatus.user (round 90; found in round 89's re-audit):
        // fetched exactly once here, right after start, never again for
        // this same container instance — matches this codebase's "zero
        // extra RPCs for a healthy container" design (round 24) since
        // build_status()'s own per-reconcile path never touches this.
        // Best-effort: a failure here is cosmetic (the field is simply
        // absent), never worth failing the whole container creation over.
        match self.container_status_details(&created.container_id).await {
            Ok(status) => {
                if let Some(linux) = status.user.and_then(|u| u.linux) {
                    self.container_users
                        .lock()
                        .unwrap()
                        .insert(restart_count_key(sandbox_id, &container.name), (linux.uid, linux.gid, linux.supplemental_groups));
                }
            }
            Err(e) => warn!(container = %container.name, error = ?e, "ContainerStatus failed; containerStatuses[].user will be left unset for this container instance"),
        }

        // Record this container's own resources so a later shared-pool
        // refresh (triggered by some *other* container's exclusive claim/
        // release) can find and update it, and — if this container itself
        // just took a new exclusive claim — sweep every other already-
        // running shared-pool container to exclude these cores now.
        let key = restart_count_key(sandbox_id, &container.name);
        self.container_resources.lock().unwrap().insert(key.clone(), (created.container_id.clone(), resources_for_record));
        self.applied_resources.lock().unwrap().insert(key, container.resources.clone().unwrap_or_default());
        if cpu_manager_key.is_some() {
            self.refresh_shared_pool_cpusets().await;
        }

        // postStart runs after the container is started; a failing hook
        // should kill+restart the container per real kubelet, but that's a
        // bigger behavior change than this pass takes on — logged and left
        // running instead (see docs/GAP_CLOSURE.md).
        if let Some(post_start) = container.lifecycle.as_ref().and_then(|l| l.post_start.as_ref()) {
            let pod_ip = self.pod_ip(sandbox_id).await.unwrap_or_default();
            if let Err(e) = self.run_lifecycle_hook(&created.container_id, &pod_ip, post_start, 30).await {
                warn!(container = %container.name, error = ?e, "postStart hook failed (container left running)");
            }
        }

        Ok(())
    }

    /// Drive `spec.initContainers` one at a time, in order — exactly
    /// kubelet's sequencing: an init container must exit zero before the
    /// next one (or the app containers) starts. Each call advances at most
    /// one step (create-if-missing, or notice the front-of-line container
    /// finished) and reports where things stand; `ensure_pod()` calls this
    /// on every reconcile until it reports `AllComplete`.
    pub(crate) async fn ensure_init_containers(
        &self,
        sandbox_id: &str,
        id: &PodId,
        pod: &Pod,
        init_containers: &[Container],
        pod_sc: Option<&PodSecurityContext>,
        restart_policy: &str,
        volumes: &HashMap<String, ResolvedVolume>,
        pull_secrets: &[String],
        service_env: &BTreeMap<String, Vec<u8>>,
        qos: QosClass,
        claim_devices: &HashMap<String, PreparedPodClaim>,
        runtime_handler: &str,
        privileged: bool,
    ) -> Result<InitProgress> {
        let running_v = ContainerState::ContainerRunning as i32;
        let exited_v = ContainerState::ContainerExited as i32;
        let existing = self.list_pod_containers(sandbox_id).await?;

        for container in init_containers {
            let existing_ctr = existing.iter().find(|c| {
                c.labels.get(CTR_NAME_LABEL).map(|n| n == &container.name).unwrap_or(false)
                    && c.labels.get(CTR_INIT_LABEL).map(|v| v == "true").unwrap_or(false)
            });

            // Native sidecar container (round 36):
            // initContainers[].restartPolicy == "Always". Unlike a regular
            // init container, this doesn't block later init/app containers
            // on its own *exit* — only on having been started at all — and
            // it restarts on exit like a normal container for the pod's
            // whole lifetime, handled right here on every reconcile rather
            // than through the app-container restart path (it's reported
            // under initContainerStatuses, not containerStatuses, matching
            // upstream).
            if container.restart_policy.as_deref() == Some("Always") {
                match sidecar_init_decision(existing_ctr.map(|c| c.state), running_v, exited_v) {
                    SidecarInitDecision::Create => {
                        let envs = self.resolve_container_env(pod, id, container, service_env).await?;
                        let attempt = self.restart_count(sandbox_id, &container.name);
                        self.create_and_start_container(
                            sandbox_id, id, container, pod_sc, volumes, pull_secrets, &envs, ContainerKind::Init, attempt, qos, claim_devices, runtime_handler, privileged,
                        )
                        .await?;
                        // Gate later containers on this one actually
                        // starting — the next reconcile (triggered by the
                        // CRI event once it's running) picks up past it.
                        return Ok(InitProgress::Waiting);
                    }
                    SidecarInitDecision::NeedsRestart => {
                        // Exited — restart it, but don't block the rest of
                        // the sequence on this restart; later containers
                        // already saw it start once.
                        let c = existing_ctr.expect("NeedsRestart only reached when a container exists");
                        self.bump_restart_count(sandbox_id, &container.name);
                        self.release_container_devices(sandbox_id, &container.name).await;
                        let mut rt = self.rt.clone();
                        let _ = rt.remove_container(RemoveContainerRequest { container_id: c.id.clone() }).await;
                        continue;
                    }
                    SidecarInitDecision::Started => continue,
                }
            }

            let exit_code = match existing_ctr {
                Some(c) if c.state == exited_v => self.container_exit_code(&c.id).await?,
                _ => 0,
            };

            match init_container_decision(existing_ctr.map(|c| c.state), running_v, exited_v, exit_code, restart_policy) {
                InitContainerDecision::Create => {
                    let envs = self.resolve_container_env(pod, id, container, service_env).await?;
                    let attempt = self.restart_count(sandbox_id, &container.name);
                    self.create_and_start_container(
                        sandbox_id, id, container, pod_sc, volumes, pull_secrets, &envs, ContainerKind::Init, attempt, qos, claim_devices, runtime_handler, privileged,
                    )
                    .await?;
                    return Ok(InitProgress::Waiting);
                }
                InitContainerDecision::Done => continue, // this init container is done — check the next one
                InitContainerDecision::Failed => {
                    return Ok(InitProgress::Failed(format!(
                        "init container {} exited with code {exit_code}",
                        container.name
                    )));
                }
                InitContainerDecision::Retry => {
                    // Allowed to retry — clear it out; the next reconcile
                    // (triggered by this very removal, via the CRI event
                    // stream) sees no existing container and creates a fresh one.
                    let c = existing_ctr.expect("Retry only reached when a container exists");
                    self.bump_restart_count(sandbox_id, &container.name);
                    self.release_container_devices(sandbox_id, &container.name).await;
                    let mut rt = self.rt.clone();
                    let _ = rt.remove_container(RemoveContainerRequest { container_id: c.id.clone() }).await;
                    return Ok(InitProgress::Waiting);
                }
                InitContainerDecision::StillRunning | InitContainerDecision::Waiting => return Ok(InitProgress::Waiting),
            }
        }
        Ok(InitProgress::AllComplete)
    }

}
