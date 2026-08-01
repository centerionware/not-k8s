use super::*;

#[async_trait]
impl PodRuntime for CriRuntime {
    async fn ensure_pod(&self, pod: &Pod) -> Result<RuntimeStatus> {
        let id = pod_id(pod);
        let found = self.find_sandbox(&id.namespace, &id.name).await?;
        let ready_state = v1::PodSandboxState::SandboxReady as i32;
        let dns = dns_config_for(pod, &self.cluster_dns, &self.cluster_domain);
        let runtime_handler = self.resolve_runtime_handler(pod).await;
        let spec = pod.spec.as_ref();
        let hostname = resolve_pod_hostname(
            spec.and_then(|s| s.hostname.as_deref()),
            spec.and_then(|s| s.subdomain.as_deref()),
            spec.and_then(|s| s.set_hostname_as_fqdn).unwrap_or(false),
            &id.name,
            &id.namespace,
            &self.cluster_domain,
        )?;
        let sysctls = pod_sysctls(spec.and_then(|s| s.security_context.as_ref()));
        // Real kubelet computes both of these before RunPodSandbox too —
        // cgroup_parent so the sandbox lands in the right QoS-scoped cgroup
        // from the start (not something CRI lets you change after the
        // fact), overhead so a RuntimeClass with declared overhead
        // (gVisor/Kata's userspace kernel cost) gets it accounted into the
        // sandbox's own resources.
        let qos = crate::eviction::qos_class(pod);
        let cgroup_parent = crate::cgroup::cgroup_parent_for(qos, &id.uid);
        let overhead = pod.spec.as_ref().and_then(|s| s.overhead.as_ref()).map(|list| resource_list_to_linux_resources(list));
        let sandbox_id = match sandbox_reuse_decision(found.as_ref().map(|(_, s)| *s), ready_state) {
            SandboxDecision::Reuse => found.unwrap().0,
            SandboxDecision::RecreateStale => {
                // The sandbox record exists but its task/pause process
                // isn't alive (e.g. this metadata survived a reboot but
                // the process didn't) — tear it down and start clean
                // instead of reusing something CreateContainer can never
                // succeed against. Best-effort: it may already be half-gone.
                let (stale_id, _) = found.unwrap();
                let mut rt = self.rt.clone();
                let _ = rt.stop_pod_sandbox(StopPodSandboxRequest { pod_sandbox_id: stale_id.clone() }).await;
                let _ = rt.remove_pod_sandbox(RemovePodSandboxRequest { pod_sandbox_id: stale_id.clone() }).await;
                self.restart_policies.lock().unwrap().remove(&stale_id);
                self.pod_uids.lock().unwrap().remove(&stale_id);
                self.sidecar_names.lock().unwrap().remove(&stale_id);
                self.clear_restart_counts(&stale_id);
                self.clear_restart_backoff(&stale_id);
                self.clear_last_terminated(&stale_id);
                self.release_sandbox_devices(&stale_id).await;
                self.run_sandbox(&id, &hostname, &sysctls, dns, runtime_handler, cgroup_parent, overhead, spec.and_then(|s| s.security_context.as_ref())).await.context("RunPodSandbox")?
            }
            SandboxDecision::CreateFresh => {
                self.run_sandbox(&id, &hostname, &sysctls, dns, runtime_handler, cgroup_parent, overhead, spec.and_then(|s| s.security_context.as_ref())).await.context("RunPodSandbox")?
            }
        };

        let restart_policy = pod
            .spec
            .as_ref()
            .and_then(|s| s.restart_policy.clone())
            .unwrap_or_else(|| "Always".to_string());
        // Recorded for status()'s event-driven path, which only gets
        // namespace+name (no Pod object) and needs this to make the same
        // Pending-vs-Succeeded call build_status() below does.
        self.restart_policies.lock().unwrap().insert(sandbox_id.clone(), restart_policy.clone());
        self.pod_uids.lock().unwrap().insert(sandbox_id.clone(), id.uid.clone());
        // Native sidecar containers (round 36): initContainers[].restartPolicy
        // == "Always". Recorded so build_labeled_container_statuses() (whose
        // event-driven callers have no Pod object) knows which init
        // containers should get real probe-based readiness folded into the
        // pod's overall Ready/ContainersReady, same as app containers.
        let sidecar_names: HashSet<String> = pod
            .spec
            .as_ref()
            .and_then(|s| s.init_containers.as_ref())
            .map(|cs| cs.iter().filter(|c| c.restart_policy.as_deref() == Some("Always")).map(|c| c.name.clone()).collect())
            .unwrap_or_default();
        self.sidecar_names.lock().unwrap().insert(sandbox_id.clone(), sidecar_names);

        let pod_sc = pod.spec.as_ref().and_then(|s| s.security_context.as_ref());
        let pull_secrets: Vec<String> = pod
            .spec
            .as_ref()
            .and_then(|s| s.image_pull_secrets.as_ref())
            .map(|refs| refs.iter().map(|r| r.name.clone()).filter(|n| !n.is_empty()).collect())
            .unwrap_or_default();

        let volumes = self.resolve_volumes(pod, &id, &pull_secrets).await;
        let service_env = if pod.spec.as_ref().and_then(|s| s.enable_service_links) == Some(false) {
            BTreeMap::new()
        } else {
            self.resolve_service_env(&id.namespace)
                .await
                .context("resolving Service environment")?
        };
        // Dynamic Resource Allocation (round 63): resolved/prepared once
        // up front, same as volumes/service_env above — a pod with no
        // resourceClaims costs nothing (see resolve_pod_claim_devices()).
        let claim_devices = self.resolve_pod_claim_devices(pod).await;

        let init_containers = pod.spec.as_ref().and_then(|s| s.init_containers.clone()).unwrap_or_default();
        if !init_containers.is_empty() {
            let progress = self
                .ensure_init_containers(
                    &sandbox_id,
                    &id,
                    pod,
                    &init_containers,
                    pod_sc,
                    &restart_policy,
                    &volumes,
                    &pull_secrets,
                    &service_env,
                    qos,
                    &claim_devices,
                )
                .await?;
            match progress {
                InitProgress::Waiting => {
                    return Ok(RuntimeStatus {
                        phase: Phase::Pending,
                        message: Some("waiting for init containers to complete".to_string()),
                        started_at: None,
                        pod_ip: None,
                        containers: Vec::new(),
                        init_containers: self
                            .build_labeled_container_statuses(&sandbox_id, &id.uid, CTR_INIT_LABEL, true)
                            .await
                            .unwrap_or_default(),
                        ephemeral_containers: Vec::new(),
                        initialized: false,
                    });
                }
                InitProgress::Failed(reason) => {
                    return Ok(RuntimeStatus {
                        phase: Phase::Failed,
                        message: Some(reason),
                        started_at: None,
                        pod_ip: None,
                        containers: Vec::new(),
                        init_containers: self
                            .build_labeled_container_statuses(&sandbox_id, &id.uid, CTR_INIT_LABEL, true)
                            .await
                            .unwrap_or_default(),
                        ephemeral_containers: Vec::new(),
                        initialized: false,
                    });
                }
                InitProgress::AllComplete => {}
            }
        }

        if let Some(spec) = pod.spec.as_ref() {
            for c in &spec.containers {
                let envs = self.resolve_container_env(pod, &id, c, &service_env).await?;
                self.ensure_container(&sandbox_id, &id, c, pod_sc, &restart_policy, &volumes, &pull_secrets, &envs, qos, &claim_devices)
                    .await?;
            }
            // Added post-hoc via the `ephemeralcontainers` subresource (e.g.
            // `kubectl debug`), never present when a pod is first created —
            // best-effort: a failure to start one debug container shouldn't
            // fail reconciling the rest of a pod that's otherwise healthy.
            for ec in spec.ephemeral_containers.as_deref().unwrap_or(&[]) {
                let container = ephemeral_to_container(ec);
                if let Err(e) = self
                    .ensure_ephemeral_container(&sandbox_id, &id, pod, &container, pod_sc, &volumes, &pull_secrets, &service_env, &claim_devices)
                    .await
                {
                    warn!(container = %ec.name, error = ?e, "failed to start ephemeral container");
                }
            }
        }

        self.build_status(&sandbox_id, &id.uid, &restart_policy).await
    }

    async fn remove_pod(&self, pod: &Pod) -> Result<()> {
        let id = pod_id(pod);
        if let Some((sandbox_id, _state)) = self.find_sandbox(&id.namespace, &id.name).await? {
            let grace = termination_grace_seconds(pod);
            self.graceful_stop_containers(&sandbox_id, pod, grace).await;

            let mut rt = self.rt.clone();
            // StopPodSandbox is idempotent; RemovePodSandbox also removes its containers.
            let _ = rt
                .stop_pod_sandbox(StopPodSandboxRequest { pod_sandbox_id: sandbox_id.clone() })
                .await;
            rt.remove_pod_sandbox(RemovePodSandboxRequest { pod_sandbox_id: sandbox_id.clone() })
                .await
                .context("RemovePodSandbox")?;
            self.restart_policies.lock().unwrap().remove(&sandbox_id);
            if let Some(pod_uid) = self.pod_uids.lock().unwrap().remove(&sandbox_id) {
                self.userns.release(&pod_uid);
            }
            self.sidecar_names.lock().unwrap().remove(&sandbox_id);
            self.clear_restart_counts(&sandbox_id);
            self.clear_restart_backoff(&sandbox_id);
            self.clear_last_terminated(&sandbox_id);
            self.release_sandbox_devices(&sandbox_id).await;
        }
        self.unmount_csi_volumes(pod, &id).await;
        unmount_special_medium_empty_dirs(pod, &id);
        self.unprepare_pod_claim_devices(pod).await;
        Ok(())
    }

    async fn status(&self, namespace: &str, name: &str) -> Result<Option<RuntimeStatus>> {
        match self.find_sandbox(namespace, name).await? {
            Some((sandbox_id, _state)) => {
                let restart_policy = self
                    .restart_policies
                    .lock()
                    .unwrap()
                    .get(&sandbox_id)
                    .cloned()
                    .unwrap_or_else(|| "Always".to_string());
                let pod_uid = self.pod_uids.lock().unwrap().get(&sandbox_id).cloned().unwrap_or_default();
                Ok(Some(self.build_status(&sandbox_id, &pod_uid, &restart_policy).await?))
            }
            None => Ok(None),
        }
    }

    fn take_event_rx(&self) -> Option<UnboundedReceiver<String>> {
        self.rx.lock().unwrap().take()
    }

    async fn exec(&self, namespace: &str, name: &str, container: &str, command: &[String]) -> Result<bool> {
        let Some((sandbox_id, _)) = self.find_sandbox(namespace, name).await? else {
            return Ok(false); // pod gone; nothing to exec into
        };
        let Some(container_id) = self.find_container_id(&sandbox_id, container).await? else {
            return Ok(false); // container gone (mid-restart); treat as a failed probe, not an error
        };
        let mut rt = self.rt.clone();
        let resp = rt
            .exec_sync(ExecSyncRequest {
                container_id,
                cmd: command.to_vec(),
                timeout: 0, // caller (probes.rs) already bounds this with its own tokio::time::timeout
            })
            .await
            .context("ExecSync")?
            .into_inner();
        Ok(resp.exit_code == 0)
    }

    async fn restart_container(&self, namespace: &str, name: &str, container: &str, grace_period_seconds: i64) -> Result<()> {
        let Some((sandbox_id, _)) = self.find_sandbox(namespace, name).await? else {
            return Ok(()); // pod already gone; nothing to restart
        };
        let Some(container_id) = self.find_container_id(&sandbox_id, container).await? else {
            return Ok(()); // already gone; the next ensure_pod() will create it fresh anyway
        };
        let mut rt = self.rt.clone();
        // Best-effort stop before remove: a container that's still alive
        // (this is a *liveness* failure, not necessarily a crash) needs to
        // actually be killed, not just have its CRI record dropped.
        // `grace_period_seconds` (round 44) honors the probe's own
        // `terminationGracePeriodSeconds` override if set, else the pod's
        // own — previously this was always a hardcoded 10s regardless of
        // either.
        let _ = rt
            .stop_container(StopContainerRequest { container_id: container_id.clone(), timeout: grace_period_seconds.max(0) })
            .await;
        rt.remove_container(RemoveContainerRequest { container_id }).await.context("RemoveContainerRequest")?;
        Ok(())
    }

    async fn gc(&self, live_pod_keys: &HashSet<String>) -> Result<()> {
        self.gc_orphaned_sandboxes(live_pod_keys).await?;
        self.gc_unreferenced_images().await?;
        Ok(())
    }

    async fn rotate_logs(&self, max_size_bytes: u64, max_files: u32) -> Result<()> {
        let running_v = ContainerState::ContainerRunning as i32;
        let mut rt = self.rt.clone();
        let containers = rt
            .list_containers(ListContainersRequest { filter: None })
            .await
            .context("ListContainers")?
            .into_inner()
            .containers;

        for c in containers {
            if c.state != running_v {
                continue; // nothing actively writing to a stopped container's log
            }
            let status = match rt.container_status(ContainerStatusRequest { container_id: c.id.clone(), verbose: false }).await {
                Ok(resp) => resp.into_inner().status,
                Err(e) => {
                    warn!(container = %c.id, error = ?e, "log rotation: ContainerStatus failed");
                    continue;
                }
            };
            let Some(log_path) = status.map(|s| s.log_path).filter(|p| !p.is_empty()) else { continue };
            let size = match std::fs::metadata(&log_path) {
                Ok(meta) => meta.len(),
                Err(_) => continue, // no log file yet, or already rotated by a previous tick
            };
            if size <= max_size_bytes {
                continue;
            }

            if let Err(e) = rotate_log_file(std::path::Path::new(&log_path), max_files) {
                warn!(container = %c.id, log_path, error = ?e, "log rotation: failed to rotate log file");
                continue;
            }
            // Tell the runtime to reopen its fd at the same path — after the
            // rename above, its existing fd would otherwise keep writing to
            // the now-renamed old file forever.
            if let Err(e) = rt.reopen_container_log(ReopenContainerLogRequest { container_id: c.id.clone() }).await {
                warn!(container = %c.id, error = ?e, "log rotation: ReopenContainerLog failed");
            }
        }
        Ok(())
    }

    async fn container_log_path(&self, namespace: &str, name: &str, container: &str) -> Result<Option<String>> {
        let Some((sandbox_id, _)) = self.find_sandbox(namespace, name).await? else { return Ok(None) };
        let Some(container_id) = self.find_container_id(&sandbox_id, container).await? else { return Ok(None) };
        let mut rt = self.rt.clone();
        let status = rt
            .container_status(ContainerStatusRequest { container_id, verbose: false })
            .await
            .context("ContainerStatus")?
            .into_inner()
            .status;
        Ok(status.map(|s| s.log_path).filter(|p| !p.is_empty()))
    }

    async fn exec_url(
        &self,
        namespace: &str,
        name: &str,
        container: &str,
        cmd: &[String],
        stdin: bool,
        stdout: bool,
        stderr: bool,
        tty: bool,
    ) -> Result<String> {
        let Some((sandbox_id, _)) = self.find_sandbox(namespace, name).await? else {
            anyhow::bail!("pod {namespace}/{name} not found")
        };
        let Some(container_id) = self.find_container_id(&sandbox_id, container).await? else {
            anyhow::bail!("container {container} not found in pod {namespace}/{name}")
        };
        let mut rt = self.rt.clone();
        let resp = rt
            .exec(ExecRequest { container_id, cmd: cmd.to_vec(), tty, stdin, stdout, stderr })
            .await
            .context("Exec")?
            .into_inner();
        Ok(resp.url)
    }

    async fn attach_url(
        &self,
        namespace: &str,
        name: &str,
        container: &str,
        stdin: bool,
        stdout: bool,
        stderr: bool,
        tty: bool,
    ) -> Result<String> {
        let Some((sandbox_id, _)) = self.find_sandbox(namespace, name).await? else {
            anyhow::bail!("pod {namespace}/{name} not found")
        };
        let Some(container_id) = self.find_container_id(&sandbox_id, container).await? else {
            anyhow::bail!("container {container} not found in pod {namespace}/{name}")
        };
        let mut rt = self.rt.clone();
        let resp = rt
            .attach(AttachRequest { container_id, stdin, tty, stdout, stderr })
            .await
            .context("Attach")?
            .into_inner();
        Ok(resp.url)
    }

    async fn port_forward_url(&self, namespace: &str, name: &str) -> Result<String> {
        let Some((sandbox_id, _)) = self.find_sandbox(namespace, name).await? else {
            anyhow::bail!("pod {namespace}/{name} not found")
        };
        let mut rt = self.rt.clone();
        let resp = rt
            .port_forward(PortForwardRequest { pod_sandbox_id: sandbox_id, port: Vec::new() })
            .await
            .context("PortForward")?
            .into_inner();
        Ok(resp.url)
    }

    async fn pod_usage_stats(&self) -> Result<Vec<crate::runtime::PodUsage>> {
        let mut rt = self.rt.clone();
        let stats = rt
            .list_pod_sandbox_stats(ListPodSandboxStatsRequest { filter: None })
            .await
            .context("ListPodSandboxStats")?
            .into_inner()
            .stats;
        Ok(stats.iter().filter_map(pod_usage_from_sandbox_stats).collect())
    }

    fn device_plugin_capacity(&self) -> BTreeMap<String, u64> {
        self.device_plugins.capacity_map()
    }

    async fn node_images(&self) -> Result<Vec<crate::runtime::NodeImage>> {
        let mut img = self.img.clone();
        let images = img.list_images(ListImagesRequest { filter: None }).await?.into_inner().images;
        Ok(images.into_iter().map(node_image_from_cri).collect())
    }

    fn mounted_csi_volumes(&self) -> Vec<(String, String)> {
        self.csi.mounted_volumes()
    }

    async fn runtime_handlers(&self) -> Result<Vec<crate::runtime::RuntimeHandlerInfo>> {
        let mut rt = self.rt.clone();
        let handlers = rt.status(StatusRequest { verbose: false }).await?.into_inner().runtime_handlers;
        Ok(handlers.into_iter().map(runtime_handler_from_cri).collect())
    }

    async fn pod_resources_snapshot(&self) -> Vec<crate::runtime::PodResourcesEntry> {
        self.build_pod_resources_snapshot().await
    }

    fn allocatable_resources(&self) -> crate::runtime::AllocatableResourcesSnapshot {
        self.build_allocatable_resources()
    }

}

/// Shift `path`, `path.1`, `path.2`, ... up by one, dropping whatever would
/// fall off the end past `max_files`, then leave `path` itself absent — the
/// caller (CRI's `ReopenContainerLog`) is what makes the runtime create a
/// fresh one. `max_files` counts the active file too, matching kubelet's
/// `--container-log-max-files` semantics (default 5: the current log plus
/// 4 rotated ones).
pub(crate) fn rotate_log_file(path: &std::path::Path, max_files: u32) -> std::io::Result<()> {
    let rotated = |n: u32| std::path::PathBuf::from(format!("{}.{n}", path.display()));
    let max_files = max_files.max(1);
    if max_files == 1 {
        // No rotated copies kept at all — just drop the oversized log.
        return std::fs::remove_file(path);
    }

    // Oldest surviving slot first: drop anything that would land past max_files.
    for n in (1..max_files).rev() {
        let from = rotated(n);
        if !from.exists() {
            continue;
        }
        if n + 1 >= max_files {
            std::fs::remove_file(&from)?;
        } else {
            std::fs::rename(&from, rotated(n + 1))?;
        }
    }
    std::fs::rename(path, rotated(1))?;
    Ok(())
}


