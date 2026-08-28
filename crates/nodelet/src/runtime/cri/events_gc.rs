use super::*;

/// Seed the controller with every nodelet-managed pod already known to CRI.
///
/// A nodelet restart has no in-memory event history, and a containerd restart
/// can leave CRI metadata for containers whose tasks no longer exist. Waiting
/// for a new task event is therefore insufficient: the controller must inspect
/// the runtime inventory as soon as it connects and run the ordinary desired-
/// state reconciliation for each affected Pod.
pub(crate) async fn seed_existing_runtime_pods(
    mut rt: RuntimeServiceClient<Channel>,
    tx: UnboundedSender<String>,
) {
    let mut keys = HashSet::new();

    match tokio::time::timeout(
        STARTUP_RPC_TIMEOUT,
        rt.list_pod_sandbox(ListPodSandboxRequest { filter: None }),
    )
    .await
    {
        Ok(Ok(response)) => {
            for sandbox in response.into_inner().items {
                if let Some(key) = pod_key_from_labels(&sandbox.labels) {
                    keys.insert(key);
                }
            }
        }
        Ok(Err(error)) => warn!(
            error = ?error,
            "CRI startup sandbox inventory failed; Pod watch reconciliation will still handle desired Pods"
        ),
        Err(_) => warn!(
            timeout_secs = STARTUP_RPC_TIMEOUT.as_secs(),
            "CRI startup sandbox inventory timed out; Pod watch reconciliation will still handle desired Pods"
        ),
    }

    match tokio::time::timeout(
        STARTUP_RPC_TIMEOUT,
        rt.list_containers(ListContainersRequest { filter: None }),
    )
    .await
    {
        Ok(Ok(response)) => {
            for container in response.into_inner().containers {
                if let Some(key) = pod_key_from_labels(&container.labels) {
                    keys.insert(key);
                }
            }
        }
        Ok(Err(error)) => warn!(
            error = ?error,
            "CRI startup container inventory failed; sandbox inventory and Pod watch reconciliation will still handle desired Pods"
        ),
        Err(_) => warn!(
            timeout_secs = STARTUP_RPC_TIMEOUT.as_secs(),
            "CRI startup container inventory timed out; sandbox inventory and Pod watch reconciliation will still handle desired Pods"
        ),
    }

    let count = keys.len();
    for key in keys {
        if tx.send(key).is_err() {
            return;
        }
    }
    info!(
        pod_count = count,
        "CRI startup inventory queued for reconciliation"
    );
}

fn pod_key_from_labels(labels: &HashMap<String, String>) -> Option<String> {
    Some(crate::runtime::pod_key(
        labels.get(POD_NS_LABEL)?,
        labels.get(POD_NAME_LABEL)?,
    ))
}

/// Event subscriber: prefer the CRI-standard `GetContainerEvents` (works on
/// containerd >= 1.7 and CRI-O); if the runtime doesn't implement it, fall back
/// to containerd's top-level `Events/Subscribe` API (present in every containerd
/// version). Either way, changed pod keys are pushed onto `tx` — no polling.
pub(crate) async fn event_loop(channel: Channel, tx: UnboundedSender<String>) {
    if run_cri_events(&channel, &tx).await == EventOutcome::Unsupported {
        info!("CRI GetContainerEvents unsupported; using containerd native events API");
        containerd_events_loop(channel, tx).await;
    }
}


#[derive(PartialEq)]
pub(crate) enum EventOutcome {
    Unsupported,
    ReceiverGone,
}


/// Returns `Unsupported` if the runtime lacks `GetContainerEvents` (caller should
/// fall back); otherwise keeps reconnecting and only returns when `tx` is closed.
pub(crate) async fn run_cri_events(channel: &Channel, tx: &UnboundedSender<String>) -> EventOutcome {
    loop {
        let mut client = RuntimeServiceClient::new(channel.clone());
        match client.get_container_events(GetEventsRequest::default()).await {
            Ok(resp) => {
                let mut stream = resp.into_inner();
                loop {
                    match stream.message().await {
                        Ok(Some(ev)) => {
                            if let Some(meta) = ev.pod_sandbox_status.and_then(|s| s.metadata) {
                                let key = crate::runtime::pod_key(&meta.namespace, &meta.name);
                                debug!(pod = %key, "CRI container event");
                                if tx.send(key).is_err() {
                                    return EventOutcome::ReceiverGone;
                                }
                            }
                        }
                        Ok(None) => break, // stream ended; reconnect
                        Err(e) => {
                            warn!(error = ?e, "CRI event stream error");
                            break;
                        }
                    }
                }
            }
            Err(e) if e.code() == tonic::Code::Unimplemented => return EventOutcome::Unsupported,
            Err(e) => warn!(error = ?e, "failed to open CRI event stream"),
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}


/// Fallback: subscribe to containerd's native event firehose in the `k8s.io`
/// namespace, watch `/tasks/*` events, map the container id back to a pod via its
/// labels, and push the pod key. Reconnects on error.
pub(crate) async fn containerd_events_loop(channel: Channel, tx: UnboundedSender<String>) {
    loop {
        let mut client = EventsClient::new(channel.clone());
        // Empty filters = whole firehose; we scope to k8s.io via the namespace
        // header and filter topics client-side (robust against filter grammar).
        let mut req = tonic::Request::new(SubscribeRequest { filters: vec![] });
        req.metadata_mut()
            .insert("containerd-namespace", "k8s.io".parse().unwrap());

        match client.subscribe(req).await {
            Ok(resp) => {
                let mut stream = resp.into_inner();
                loop {
                    match stream.message().await {
                        Ok(Some(env)) => {
                            if !env.topic.starts_with("/tasks/") {
                                continue;
                            }
                            let Some(cid) = env
                                .event
                                .and_then(|a| TaskEventContainerId::decode(a.value.as_slice()).ok())
                                .map(|t| t.container_id)
                                .filter(|c| !c.is_empty())
                            else {
                                continue;
                            };
                            debug!(topic = %env.topic, container = %cid, "containerd task event");
                            if let Some((ns, name)) = lookup_pod_by_cid(channel.clone(), &cid).await {
                                if tx.send(crate::runtime::pod_key(&ns, &name)).is_err() {
                                    return; // controller dropped the receiver
                                }
                            }
                        }
                        Ok(None) => break,
                        Err(e) => {
                            warn!(error = ?e, "containerd event stream error");
                            break;
                        }
                    }
                }
            }
            Err(e) => warn!(error = ?e, "failed to subscribe to containerd events"),
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}


/// Map a containerd container/sandbox id back to its pod (namespace, name) via
/// the `nodelet.dev/*` labels we stamped on it.
pub(crate) async fn lookup_pod_by_cid(channel: Channel, cid: &str) -> Option<(String, String)> {
    fn ns_name(labels: &HashMap<String, String>) -> Option<(String, String)> {
        Some((labels.get(POD_NS_LABEL)?.clone(), labels.get(POD_NAME_LABEL)?.clone()))
    }

    // App containers first.
    let mut rt = RuntimeServiceClient::new(channel.clone());
    if let Ok(resp) = rt
        .list_containers(ListContainersRequest {
            filter: Some(ContainerFilter { id: cid.to_string(), ..Default::default() }),
        })
        .await
    {
        if let Some(c) = resp.into_inner().containers.into_iter().next() {
            if let Some(p) = ns_name(&c.labels) {
                return Some(p);
            }
        }
    }

    // Otherwise the id may be a pod sandbox (e.g. the pause container's task).
    let mut rt = RuntimeServiceClient::new(channel);
    if let Ok(resp) = rt
        .list_pod_sandbox(ListPodSandboxRequest {
            filter: Some(PodSandboxFilter { id: cid.to_string(), ..Default::default() }),
        })
        .await
    {
        if let Some(s) = resp.into_inner().items.into_iter().next() {
            return ns_name(&s.labels);
        }
    }
    None
}


impl CriRuntime {
    /// Every nodelet-managed sandbox on the node, unfiltered by pod — the
    /// `find_sandbox()` lookups elsewhere always scope to one pod; GC needs
    /// the reverse view (every sandbox, checked against the apiserver).
    pub(crate) async fn list_all_sandboxes(&self) -> Result<Vec<(String, String, String)>> {
        let mut rt = self.rt.clone();
        let resp = rt.list_pod_sandbox(ListPodSandboxRequest { filter: None }).await?.into_inner();
        Ok(resp
            .items
            .into_iter()
            .filter_map(|s| Some((s.labels.get(POD_NS_LABEL)?.clone(), s.labels.get(POD_NAME_LABEL)?.clone(), s.id)))
            .collect())
    }

    pub(crate) async fn gc_orphaned_sandboxes(&self, live_pod_keys: &HashSet<String>) -> Result<()> {
        let sandboxes = self.list_all_sandboxes().await?;
        let orphans = crate::gc::orphaned_sandboxes(&sandboxes, live_pod_keys);
        for sandbox_id in orphans {
            info!(sandbox = %sandbox_id, "gc: removing orphaned sandbox (pod no longer in apiserver)");
            let mut rt = self.rt.clone();
            let _ = rt.stop_pod_sandbox(StopPodSandboxRequest { pod_sandbox_id: sandbox_id.clone() }).await;
            if let Err(e) = rt.remove_pod_sandbox(RemovePodSandboxRequest { pod_sandbox_id: sandbox_id.clone() }).await {
                warn!(sandbox = %sandbox_id, error = ?e, "gc: failed to remove orphaned sandbox");
            }
            self.restart_policies.lock().unwrap().remove(&sandbox_id);
            if let Some(pod_uid) = self.pod_uids.lock().unwrap().remove(&sandbox_id) {
                self.userns.release(&pod_uid);
            }
            self.sidecar_names.lock().unwrap().remove(&sandbox_id);
            self.clear_restart_counts(&sandbox_id);
            self.clear_restart_backoff(&sandbox_id);
            self.clear_pull_backoff(&sandbox_id);
            self.clear_config_errors(&sandbox_id);
            self.clear_last_terminated(&sandbox_id);
            self.release_sandbox_devices(&sandbox_id).await;
        }
        Ok(())
    }

    /// Image GC (round 70; found in round 69's fresh gap re-audit):
    /// real kubelet's own watermark policy, not an unconditional
    /// unreferenced-image sweep on every cycle (the pre-round-70
    /// behavior). An unreferenced image is left alone — regardless of
    /// how long it's sat there — unless `disk_path`'s usage has crossed
    /// `image_gc_high_threshold_percent`; once triggered, only images
    /// unreferenced for at least `image_gc_min_age_secs` are eligible,
    /// removed oldest-unreferenced-first until usage drops to
    /// `image_gc_low_threshold_percent` or nothing eligible remains.
    pub(crate) async fn gc_unreferenced_images(&self) -> Result<()> {
        let mut rt = self.rt.clone();
        let containers = rt
            .list_containers(ListContainersRequest { filter: None })
            .await?
            .into_inner()
            .containers;
        let referenced: HashSet<String> = containers
            .into_iter()
            .filter_map(|c| c.image.map(|i| i.image))
            .collect();

        let mut img = self.img.clone();
        let images = img.list_images(ListImagesRequest { filter: None }).await?.into_inner().images;
        let refs: Vec<crate::gc::ImageRef> = images
            .into_iter()
            .map(|i| crate::gc::ImageRef { id: i.id, repo_tags: i.repo_tags, repo_digests: i.repo_digests, size_bytes: i.size })
            .collect();

        let unreferenced_ids: HashSet<String> = crate::gc::images_to_gc(&refs, &referenced).into_iter().collect();

        let now_secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
        let unreferenced_since = {
            let mut tracked = self.image_unreferenced_since.lock().unwrap();
            tracked.retain(|id, _| unreferenced_ids.contains(id));
            for id in &unreferenced_ids {
                tracked.entry(id.clone()).or_insert(now_secs);
            }
            tracked.clone()
        };

        let Some(disk) = crate::metrics::read_disk_info(&self.disk_path) else {
            // Can't measure usage at all — fail open (skip this cycle)
            // rather than guessing, same posture as DiskPressure's own
            // statvfs read failure.
            return Ok(());
        };
        let usage_percent = crate::gc::disk_usage_percent(disk.total_bytes, disk.available_bytes);
        if !crate::gc::should_start_image_gc(usage_percent, self.image_gc_high_threshold_percent) {
            return Ok(());
        }

        let candidates: Vec<crate::gc::ImageRef> = refs.into_iter().filter(|r| unreferenced_ids.contains(&r.id)).collect();
        let to_remove = crate::gc::images_to_reclaim_space(
            &candidates,
            &unreferenced_since,
            now_secs,
            self.image_gc_min_age_secs,
            disk.total_bytes,
            disk.available_bytes,
            self.image_gc_low_threshold_percent,
        );

        for image_id in to_remove {
            info!(image = %image_id, usage_percent, "gc: removing image to reclaim disk space (image GC high watermark exceeded)");
            let image_spec = ImageSpec { image: image_id.clone(), ..Default::default() };
            match img.remove_image(RemoveImageRequest { image: Some(image_spec) }).await {
                Ok(_) => {
                    self.image_unreferenced_since.lock().unwrap().remove(&image_id);
                }
                Err(e) => warn!(image = %image_id, error = ?e, "gc: failed to remove image"),
            }
        }
        Ok(())
    }

}
