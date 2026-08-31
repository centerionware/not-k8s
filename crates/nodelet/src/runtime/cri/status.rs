use super::*;

/// Where a container's `terminationMessagePath` file lives on the host —
/// bind-mounted into the container at container-creation time (see
/// `create_and_start_container()`) so nodelet can read it back after the
/// container exits without needing any runtime cooperation, the same
/// approach real kubelet itself uses (not a CRI-level concept at all).
pub(crate) fn termination_message_host_path(pod_uid: &str, container_name: &str) -> PathBuf {
    PathBuf::from(VOLUME_ROOT).join(pod_uid).join("termination-log").join(container_name)
}


/// Read a termination-message file's content, capped at
/// `MAX_TERMINATION_MESSAGE_BYTES` — keeping the *last* bytes if the file
/// is larger (a container's most recent write), not the first. Empty (not
/// an error) for a missing/unreadable file — the common case: most
/// containers never write one at all.
pub(crate) fn read_termination_message(path: &std::path::Path) -> String {
    match std::fs::read(path) {
        Ok(bytes) => {
            let start = bytes.len().saturating_sub(MAX_TERMINATION_MESSAGE_BYTES);
            String::from_utf8_lossy(&bytes[start..]).into_owned()
        }
        Err(_) => String::new(),
    }
}


/// CRI's `Image` (`repo_tags`/`repo_digests`/`size`) -> `Node.status.images`'
/// shape (`names` combining both, `size_bytes`) — pure so the combination
/// is unit-testable without a real image cache.
pub(crate) fn node_image_from_cri(image: v1::Image) -> crate::runtime::NodeImage {
    let mut names = image.repo_tags;
    names.extend(image.repo_digests);
    crate::runtime::NodeImage { names, size_bytes: image.size }
}


/// Real kubelet's `<runtimeName>://<id>` container-ID format (round 57;
/// found in round 54's re-audit) — applied to `ContainerRuntimeStatus.container_id`
/// right where it's populated from CRI's own bare ID, so every downstream
/// consumer (`ContainerStatus.containerID`, `state.terminated.containerID`
/// — both read the same field) gets the prefix without needing its own
/// formatting logic.
pub(crate) fn format_container_id(runtime_name: &str, id: &str) -> String {
    format!("{runtime_name}://{id}")
}


/// CRI's `RuntimeHandler` -> `Node.status.runtimeHandlers`' shape (round
/// 53) — pure so the field mapping is unit-testable without a real CRI
/// socket.
pub(crate) fn runtime_handler_from_cri(h: v1::RuntimeHandler) -> crate::runtime::RuntimeHandlerInfo {
    let features = h.features.unwrap_or_default();
    crate::runtime::RuntimeHandlerInfo {
        name: h.name,
        recursive_read_only_mounts: features.recursive_read_only_mounts,
        user_namespaces: features.user_namespaces,
    }
}


pub(crate) fn u64_value(v: &Option<v1::UInt64Value>) -> Option<u64> {
    v.as_ref().map(|v| v.value)
}


pub(crate) fn usage_stats_from_cpu_memory(cpu: Option<&v1::CpuUsage>, memory: Option<&v1::MemoryUsage>) -> crate::runtime::UsageStats {
    crate::runtime::UsageStats {
        cpu_usage_nano_cores: cpu.and_then(|c| u64_value(&c.usage_nano_cores)),
        cpu_usage_core_nano_seconds: cpu.and_then(|c| u64_value(&c.usage_core_nano_seconds)),
        memory_working_set_bytes: memory.and_then(|m| u64_value(&m.working_set_bytes)),
        memory_usage_bytes: memory.and_then(|m| u64_value(&m.usage_bytes)),
        memory_rss_bytes: memory.and_then(|m| u64_value(&m.rss_bytes)),
        memory_available_bytes: memory.and_then(|m| u64_value(&m.available_bytes)),
    }
}


/// Sum of every container's CRI-reported writable-layer usage (round 49)
/// — the container's own filesystem writes (anything not on a mounted
/// volume), the piece of ephemeral-storage usage nodelet has no other way
/// to measure itself (containerd owns that storage, not nodelet).
pub(crate) fn writable_layer_bytes(containers: &[v1::ContainerStats]) -> u64 {
    containers.iter().filter_map(|c| c.writable_layer.as_ref().and_then(|w| u64_value(&w.used_bytes))).sum()
}


/// Recursive directory size, in bytes — nodelet's own materialized volume
/// directory (round 49) is the other half of a pod's ephemeral-storage
/// usage: emptyDir/ConfigMap/Secret/downwardAPI/projected content nodelet
/// itself writes, which containerd's own stats never account for since
/// they're not part of any container's writable layer. `0` on any error
/// (missing directory — most pods have no such volumes at all — or a
/// permission issue), matching this file's existing "best-effort,
/// fail-open" posture for filesystem reads.
pub(crate) fn directory_usage_bytes(path: &std::path::Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(path) else { return 0 };
    let mut total = 0u64;
    for entry in entries.flatten() {
        match entry.metadata() {
            Ok(meta) if meta.is_dir() => total += directory_usage_bytes(&entry.path()),
            Ok(meta) => total += meta.len(),
            Err(_) => {}
        }
    }
    total
}


/// Convert one CRI `PodSandboxStats` into nodelet's runtime-agnostic
/// `PodUsage`. `None` if the sandbox has no identifying metadata or no
/// Linux stats attached (Windows stats, or a sandbox CRI hasn't measured
/// yet) — nothing meaningful to report either way.
pub(crate) fn pod_usage_from_sandbox_stats(stats: &v1::PodSandboxStats) -> Option<crate::runtime::PodUsage> {
    let attrs = stats.attributes.as_ref()?;
    let metadata = attrs.metadata.as_ref()?;
    let linux = stats.linux.as_ref()?;

    let containers = linux
        .containers
        .iter()
        .filter_map(|c| {
            let name = c.attributes.as_ref()?.metadata.as_ref()?.name.clone();
            Some(crate::runtime::ContainerUsage {
                name,
                stats: usage_stats_from_cpu_memory(c.cpu.as_ref(), c.memory.as_ref()),
            })
        })
        .collect();

    // Local ephemeral storage (round 49): container writable layers
    // (containerd's own stats) plus nodelet's own materialized volume
    // directory (not part of any writable layer). Known scope
    // limitation: doesn't include container log file size
    // (/var/log/pods/...) — see PodUsage's own doc comment.
    let volume_dir = PathBuf::from(VOLUME_ROOT).join(&metadata.uid).join("volumes");
    let ephemeral_storage_usage_bytes =
        Some(writable_layer_bytes(&linux.containers) + directory_usage_bytes(&volume_dir));

    // Per-volume usage (round 67; feeds emptyDir.sizeLimit enforcement)
    // — every immediate subdirectory of volume_dir is one
    // spec.volumes[].name, regardless of kind; the eviction-side check
    // only ever looks up the emptyDir ones that actually have a
    // sizeLimit set, so measuring every volume here unconditionally
    // (rather than first figuring out which are emptyDir) keeps this
    // function decoupled from the Pod spec entirely, same as
    // ephemeral_storage_usage_bytes above.
    let empty_dir_usage_bytes: HashMap<String, u64> = std::fs::read_dir(&volume_dir)
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .map(|e| (e.file_name().to_string_lossy().into_owned(), directory_usage_bytes(&e.path())))
        .collect();

    // Network I/O (round 102): pod-scoped (one shared netns per pod), read
    // straight off the same PodSandboxStats this function already parses
    // everything else from — no extra RPC.
    let default_interface = linux.network.as_ref().and_then(|n| n.default_interface.as_ref());
    let network_interface = default_interface.map(|i| i.name.clone());
    let network_rx_bytes = default_interface.and_then(|i| u64_value(&i.rx_bytes));
    let network_tx_bytes = default_interface.and_then(|i| u64_value(&i.tx_bytes));

    Some(crate::runtime::PodUsage {
        namespace: metadata.namespace.clone(),
        name: metadata.name.clone(),
        uid: metadata.uid.clone(),
        pod: usage_stats_from_cpu_memory(linux.cpu.as_ref(), linux.memory.as_ref()),
        containers,
        ephemeral_storage_usage_bytes,
        empty_dir_usage_bytes,
        network_interface,
        network_rx_bytes,
        network_tx_bytes,
    })
}


pub(crate) fn sandbox_labels(id: &PodId) -> HashMap<String, String> {
    let mut labels = HashMap::from([
        (POD_UID_LABEL.to_string(), id.uid.clone()),
        (POD_NAME_LABEL.to_string(), id.name.clone()),
        (POD_NS_LABEL.to_string(), id.namespace.clone()),
    ]);
    if id.host_network {
        labels.insert(HOST_NETWORK_LABEL.to_string(), "true".to_string());
    }
    labels
}

pub(crate) const HOST_NETWORK_LABEL: &str = "nodelet.dev/host-network";

impl CriRuntime {
    pub(crate) async fn pod_ip(&self, sandbox_id: &str) -> Option<String> {
        let mut rt = self.rt.clone();
        let resp = match tokio::time::timeout(
            STARTUP_RPC_TIMEOUT,
            rt.pod_sandbox_status(PodSandboxStatusRequest {
                pod_sandbox_id: sandbox_id.to_string(),
                verbose: false,
            }),
        )
        .await
        {
            Ok(Ok(resp)) => resp.into_inner(),
            Ok(Err(error)) => {
                warn!(sandbox = %sandbox_id, ?error, "PodSandboxStatus failed while resolving Pod IP");
                return None;
            }
            Err(_) => {
                warn!(sandbox = %sandbox_id, timeout_secs = STARTUP_RPC_TIMEOUT.as_secs(), "PodSandboxStatus timed out while resolving Pod IP");
                return None;
            }
        };
        let Some(status) = resp.status else {
            warn!(sandbox = %sandbox_id, "PodSandboxStatus returned no status while resolving Pod IP");
            return None;
        };
        // CRI runtimes commonly report 127.0.0.1 (or no network entry at
        // all) for a sandbox sharing the node network namespace. Kubernetes
        // exposes the node's InternalIP as the Pod IP in that case, and the
        // nodelet advertises that same value in Node.status. Keep this
        // scoped to host-network sandboxes; a broad fallback would hide a
        // genuinely broken CNI assignment for ordinary Pods.
        if status
            .labels
            .get(HOST_NETWORK_LABEL)
            .is_some_and(|value| value == "true")
        {
            return Some(crate::node::detect_internal_ip());
        }
        if let Some(ip) = status
            .network
            .as_ref()
            .and_then(|network| (!network.ip.is_empty()).then_some(network.ip.clone()))
            .or_else(|| {
                status
                    .network
                    .as_ref()?
                    .additional_ips
                    .iter()
                    .find_map(|ip| (!ip.ip.is_empty()).then_some(ip.ip.clone()))
            })
        {
            return Some(ip);
        }

        // containerd 2.x's CRI plugin persists the CNI result in the
        // verbose sandbox-info payload as well as its normal Network field.
        // A runtime restart or a sandbox-controller implementation can leave
        // the latter temporarily empty even though the namespace and CNI
        // result are healthy. Recover that IP without weakening the normal
        // CRI contract or making every healthy status call pay for verbose
        // metadata.
        let mut rt = self.rt.clone();
        let verbose = match tokio::time::timeout(
            STARTUP_RPC_TIMEOUT,
            rt.pod_sandbox_status(PodSandboxStatusRequest {
                pod_sandbox_id: sandbox_id.to_string(),
                verbose: true,
            }),
        )
        .await
        {
            Ok(Ok(resp)) => resp.into_inner(),
            Ok(Err(error)) => {
                warn!(sandbox = %sandbox_id, ?error, "verbose PodSandboxStatus failed while recovering Pod IP");
                return None;
            }
            Err(_) => {
                warn!(sandbox = %sandbox_id, timeout_secs = STARTUP_RPC_TIMEOUT.as_secs(), "verbose PodSandboxStatus timed out while recovering Pod IP");
                return None;
            }
        };
        let ip = verbose
            .info
            .get("info")
            .and_then(|info| serde_json::from_str::<serde_json::Value>(info).ok())
            .and_then(|info| cni_result_ip(&info));
        if let Some(ip) = ip.as_ref() {
            debug!(sandbox = %sandbox_id, %ip, "recovered Pod IP from verbose CRI sandbox metadata");
        } else {
            warn!(sandbox = %sandbox_id, state = status.state, "CRI sandbox status has no Pod IP");
        }
        ip
    }

    pub(crate) async fn build_status(&self, sandbox_id: &str, pod_uid: &str, restart_policy: &str) -> Result<RuntimeStatus> {
        // Init containers are excluded here — by the time app containers are
        // even started, every init container has already exited zero
        // (ensure_init_containers() gates on that), so counting them would
        // make `all_exited` true for entirely the wrong reason.
        let mut containers: Vec<_> = self
            .list_pod_containers(sandbox_id)
            .await?
            .into_iter()
            .filter(|c| !c.labels.contains_key(CTR_INIT_LABEL) && !c.labels.contains_key(CTR_EPHEMERAL_LABEL))
            .collect();
        // Same ordering fix as build_labeled_container_statuses() below —
        // CRI's ListContainers makes no ordering guarantee, so
        // containerStatuses needs the same created_at sort to reliably
        // come back in spec order.
        containers.sort_by_key(|c| c.created_at);
        let running_v = ContainerState::ContainerRunning as i32;
        let exited_v = ContainerState::ContainerExited as i32;

        let mut crs = Vec::new();
        let mut any_running = false;
        let mut all_exited = !containers.is_empty();
        // Unlike the old exit-code-only check this replaces (which only
        // ever ran for restartPolicy: Never, and only once every container
        // had exited), terminated-state details below are now fetched for
        // *any* individual exited container regardless of restart policy or
        // sibling state — real value for a crash-looping Always-restart
        // container (kubectl describe should show *why* it last died), not
        // just Job-style completion. Still bounded to "no longer running"
        // containers only, so a healthy steady-state pod pays zero extra
        // RPCs, matching this codebase's low-idle-cost design throughout.
        let mut any_failed = false;
        let mut earliest_created = i64::MAX;

        for c in &containers {
            let running = c.state == running_v;
            let exited = c.state == exited_v;
            any_running |= running;
            all_exited &= exited;
            earliest_created = earliest_created.min(c.created_at);
            let name = c.metadata.as_ref().map(|m| m.name.clone()).unwrap_or_default();

            let (exit_code, reason, finished_at, termination_message, stop_signal, live_last_terminated) = if exited {
                match self.container_status_details(&c.id).await {
                    Ok(details) => {
                        if details.exit_code != 0 {
                            any_failed = true;
                        }
                        let reason = if !details.reason.is_empty() {
                            details.reason
                        } else if details.exit_code == 0 {
                            "Completed".to_string()
                        } else {
                            "Error".to_string()
                        };
                        let finished_at = (details.finished_at > 0)
                            .then(|| Timestamp::from_nanosecond(details.finished_at as i128).ok())
                            .flatten();
                        let message = read_termination_message(&termination_message_host_path(pod_uid, &name));
                        let stop_signal = stop_signal_k8s(details.stop_signal);
                        // Crash-loop backoff (round 73)/lastState (round
                        // 75): while this exact instance is sitting exited
                        // and waiting out its backoff window (restartPolicy:
                        // Never never reaches this — no backoff state is
                        // ever recorded for it, see container_create.rs), it
                        // reports Waiting{CrashLoopBackOff} as its *current*
                        // state instead of Terminated, with these same
                        // just-fetched details moved into lastState — this
                        // instance hasn't actually been replaced yet, so the
                        // persisted last_terminated table (which only
                        // updates at actual replacement time) doesn't have
                        // it yet either.
                        let backing_off = restart_policy != "Never" && !self.restart_backoff_ready(sandbox_id, &name);
                        let live_last_terminated = backing_off.then(|| crate::runtime::TerminatedInfo {
                            container_id: Some(format_container_id(&self.runtime_name, &c.id)),
                            exit_code: details.exit_code,
                            reason: reason.clone(),
                            finished_at,
                            message: message.clone(),
                        });
                        if backing_off {
                            (None, String::new(), None, String::new(), stop_signal, live_last_terminated)
                        } else {
                            (Some(details.exit_code), reason, finished_at, message, stop_signal, None)
                        }
                    }
                    Err(e) => {
                        warn!(container = %c.id, error = ?e, "ContainerStatus failed; reporting this exited container without terminated details");
                        (None, String::new(), None, String::new(), None, None)
                    }
                }
            } else {
                (None, String::new(), None, String::new(), None, None)
            };
            let waiting_reason_override = live_last_terminated.is_some().then(|| "CrashLoopBackOff".to_string());
            let last_terminated = live_last_terminated.or_else(|| self.last_terminated_for(sandbox_id, &name));

            let resource_key = restart_count_key(sandbox_id, &name);
            let resources = self.applied_resources.lock().unwrap().get(&resource_key).cloned();
            let allocated_resources = self.spec_resources.lock().unwrap().get(&resource_key).cloned();

            crs.push(ContainerRuntimeStatus {
                name: name.clone(),
                image: c.image.as_ref().map(|i| i.image.clone()).unwrap_or_default(),
                image_id: c.image_ref.clone(),
                ready: running,
                running,
                container_id: Some(format_container_id(&self.runtime_name, &c.id)),
                restart_count: self.restart_count(sandbox_id, &name),
                exit_code,
                reason,
                finished_at,
                termination_message,
                is_restartable_sidecar: false, // app containers, never a sidecar concept
                resources,
                allocated_resources,
                stop_signal,
                last_terminated,
                waiting_reason_override,
                allocated_resources_status: self.allocated_resources_status(sandbox_id, &name),
                container_user: self.container_user_for(sandbox_id, &name),
                volume_mount_statuses: self.container_volume_mount_statuses_for(sandbox_id, &name),
            });
        }

        // The pod's phase must only treat a nonzero exit as terminal
        // failure under restartPolicy: Never — Always/OnFailure exits
        // aren't final, ensure_container() above just restarted them.
        let phase_failed = any_failed && all_exited && restart_policy == "Never";
        let phase = compute_phase(any_running, all_exited, phase_failed, restart_policy);

        let started_at = (earliest_created != i64::MAX && earliest_created > 0)
            .then(|| Timestamp::from_nanosecond(earliest_created as i128).ok())
            .flatten();

        Ok(RuntimeStatus {
            phase,
            message: None,
            started_at,
            pod_ip: self.pod_ip(sandbox_id).await,
            containers: crs,
            init_containers: self.build_labeled_container_statuses(sandbox_id, pod_uid, CTR_INIT_LABEL, true).await.unwrap_or_default(),
            ephemeral_containers: self
                .build_labeled_container_statuses(sandbox_id, pod_uid, CTR_EPHEMERAL_LABEL, false)
                .await
                .unwrap_or_default(),
            initialized: true,
        })
    }

    /// `ContainerRuntimeStatus` for every container in the sandbox carrying
    /// `label` (either `CTR_INIT_LABEL` or `CTR_EPHEMERAL_LABEL`) — the
    /// counterpart to the main loop in `build_status()` above, kept separate
    /// because both init and ephemeral containers are excluded from that one
    /// (their exit is expected/irrelevant, not something `all_exited` should
    /// key the *pod's* phase off). `fetch_details` gates the same
    /// terminated-state enrichment `build_status()`'s main loop does — `true`
    /// for init containers (a failed init container's exit reason matters,
    /// same as app containers), `false` for ephemeral/debug containers
    /// (round 8's existing, still-documented simplification: exit codes
    /// aren't tracked for those at all, `pods.rs` hardcodes `exit_code: 0`
    /// regardless of what's fetched here, so there's no reason to pay the
    /// extra `ContainerStatus` RPC for them).
    pub(crate) async fn build_labeled_container_statuses(
        &self,
        sandbox_id: &str,
        pod_uid: &str,
        label: &str,
        fetch_details: bool,
    ) -> Result<Vec<ContainerRuntimeStatus>> {
        let running_v = ContainerState::ContainerRunning as i32;
        let exited_v = ContainerState::ContainerExited as i32;
        let mut containers = self.list_pod_containers(sandbox_id).await?;
        // Round 123: CRI's ListContainers makes no ordering guarantee at
        // all — confirmed live, this containerd returned init containers
        // in reverse-of-creation order, so status.initContainerStatuses
        // came back as `init-two init-one` for a pod whose spec declared
        // `init-one` first. Real kubelet's own contract (and what this
        // suite's test_init_containers_run_before_app_container checks)
        // is that initContainerStatuses reflects spec order. `created_at`
        // (nanoseconds) is a real per-container CRI field set by the
        // runtime itself, not something nodelet has to track separately
        // — sorting by it recovers real creation order, which for
        // init/ephemeral containers (both run through this same
        // function) is exactly spec/attach order, since nodelet only
        // ever creates the next one after the previous one is already
        // running or done.
        containers.sort_by_key(|c| c.created_at);
        let sidecar_names = self.sidecar_names.lock().unwrap().get(sandbox_id).cloned().unwrap_or_default();
        let mut out = Vec::new();
        for c in containers.into_iter().filter(|c| c.labels.contains_key(label)) {
                let running = c.state == running_v;
                let name = c.metadata.as_ref().map(|m| m.name.clone()).unwrap_or_default();
                let is_restartable_sidecar = sidecar_names.contains(&name);
                let (exit_code, reason, finished_at, termination_message, stop_signal) = if fetch_details && c.state == exited_v {
                    match self.container_status_details(&c.id).await {
                        Ok(details) => {
                            let reason = if !details.reason.is_empty() {
                                details.reason
                            } else if details.exit_code == 0 {
                                "Completed".to_string()
                            } else {
                                "Error".to_string()
                            };
                            let finished_at = (details.finished_at > 0)
                                .then(|| Timestamp::from_nanosecond(details.finished_at as i128).ok())
                                .flatten();
                            let message = read_termination_message(&termination_message_host_path(pod_uid, &name));
                            let stop_signal = stop_signal_k8s(details.stop_signal);
                            (Some(details.exit_code), reason, finished_at, message, stop_signal)
                        }
                        Err(e) => {
                            warn!(container = %c.id, error = ?e, "ContainerStatus failed; reporting this exited container without terminated details");
                            (None, String::new(), None, String::new(), None)
                        }
                    }
                } else {
                    (None, String::new(), None, String::new(), None)
                };
                let allocated_resources_status = self.allocated_resources_status(sandbox_id, &name);
                let container_user = self.container_user_for(sandbox_id, &name);
                let volume_mount_statuses = self.container_volume_mount_statuses_for(sandbox_id, &name);
                out.push(ContainerRuntimeStatus {
                    restart_count: self.restart_count(sandbox_id, &name),
                    name,
                    image: c.image.as_ref().map(|i| i.image.clone()).unwrap_or_default(),
                    image_id: c.image_ref.clone(),
                    ready: running,
                    running,
                    container_id: Some(format_container_id(&self.runtime_name, &c.id)),
                    exit_code,
                    reason,
                    finished_at,
                    termination_message,
                    is_restartable_sidecar,
                    // Resize status reporting (round 43) is scoped to app
                    // containers only this round — init/ephemeral containers
                    // don't get a resize decision at all yet either.
                    resources: None,
                    allocated_resources: None,
                    stop_signal,
                    // Crash-loop backoff (round 73) only ever applies to
                    // app containers (ensure_container()'s own restart
                    // path) — init/ephemeral containers have no backoff
                    // state to report here, a documented scope limitation
                    // matching round 73's own.
                    last_terminated: None,
                    waiting_reason_override: None,
                    allocated_resources_status,
                    container_user,
                    volume_mount_statuses,
                });
        }
        Ok(out)
    }

}

/// Extract the first non-loopback IP from containerd's verbose CRI sandbox
/// info. This is deliberately a fallback for runtimes that expose the CNI
/// result there but omit `PodSandboxStatus.network.ip`; the standard CRI
/// status field remains authoritative whenever it is populated.
fn cni_result_ip(info: &serde_json::Value) -> Option<String> {
    let interfaces = info.pointer("/cniResult/Interfaces")?.as_object()?;
    let mut names: Vec<_> = interfaces.keys().collect();
    names.sort_by_key(|name| if *name == "eth0" { 0 } else { 1 });
    names
        .into_iter()
        .filter_map(|name| interfaces.get(name))
        .filter_map(|interface| interface.get("IPConfigs").and_then(serde_json::Value::as_array))
        .flatten()
        .filter_map(|config| config.get("IP").and_then(serde_json::Value::as_str))
        .find_map(|ip| {
            let parsed = ip.parse::<std::net::IpAddr>().ok()?;
            (!parsed.is_loopback()).then(|| ip.to_string())
        })
}

#[cfg(test)]
mod tests {
    use super::cni_result_ip;
    use serde_json::json;

    #[test]
    fn cni_result_ip_prefers_eth0_and_ignores_loopback() {
        let info = json!({
            "cniResult": {
                "Interfaces": {
                    "lo": {"IPConfigs": [{"IP": "127.0.0.1"}]},
                    "eth0": {"IPConfigs": [{"IP": "10.42.0.7"}]},
                    "eth1": {"IPConfigs": [{"IP": "10.42.0.8"}]}
                }
            }
        });

        assert_eq!(cni_result_ip(&info).as_deref(), Some("10.42.0.7"));
    }

    #[test]
    fn cni_result_ip_returns_none_without_a_non_loopback_address() {
        let info = json!({
            "cniResult": {
                "Interfaces": {
                    "lo": {"IPConfigs": [{"IP": "127.0.0.1"}, {"IP": "::1"}]}
                }
            }
        });

        assert_eq!(cni_result_ip(&info), None);
    }
}
