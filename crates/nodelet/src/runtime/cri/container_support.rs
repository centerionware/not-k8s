use super::*;

/// The mutable tag on an image reference, if any — `None` for a bare
/// digest reference (`repo@sha256:...`) or a plain repo name with no tag
/// at all. Only looks at the segment after the last `/`, so a registry
/// host:port (e.g. `myregistry.io:5000/nginx:1.25`) is never mistaken for
/// a tag separator.
pub(crate) fn image_tag(image: &str) -> Option<&str> {
    if image.contains('@') {
        return None;
    }
    let repo_start = image.rfind('/').map(|i| i + 1).unwrap_or(0);
    let tail = &image[repo_start..];
    tail.rfind(':').map(|i| &tail[i + 1..])
}


/// `imagePullPolicy` (round 51; found in round 50's re-audit), including
/// real kubelet's own default-policy heuristic when unset: `Always` for
/// an untagged or `:latest`-tagged image (a floating reference that could
/// have changed since it was last pulled), `IfNotPresent` for anything
/// else (a specific version tag, or a digest — both immutable by
/// definition, so there's nothing to gain from re-checking the registry
/// every time).
pub(crate) fn effective_pull_policy<'a>(policy: Option<&'a str>, image: &str) -> &'a str {
    match policy {
        Some(p @ ("Always" | "IfNotPresent" | "Never")) => p,
        _ if image.contains('@') => "IfNotPresent", // digest-pinned: immutable, nothing to gain from re-checking
        _ => match image_tag(image) {
            None | Some("latest") => "Always",
            Some(_) => "IfNotPresent",
        },
    }
}


/// The ServiceAccount a Pod runs as — `default` when unset, matching real
/// Kubernetes (every namespace has an auto-created `default` ServiceAccount).
pub(crate) fn pod_service_account_name(pod: &Pod) -> String {
    pod.spec
        .as_ref()
        .and_then(|s| s.service_account_name.clone())
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| "default".to_string())
}


/// `ServiceAccountTokenProjection.audience` is a single optional string;
/// `TokenRequestSpec.audiences` wants a `Vec` (empty meaning "apiserver's
/// own default audience").
pub(crate) fn token_audiences(audience: Option<&str>) -> Vec<String> {
    audience.filter(|a| !a.is_empty()).map(|a| vec![a.to_string()]).unwrap_or_default()
}


/// The registry host an image reference pulls from, e.g. `myregistry.io:5000`
/// from `myregistry.io:5000/team/app:v1`, or `docker.io` for an unqualified
/// ref like `busybox:latest` (Docker Hub's implicit default registry).
pub(crate) fn registry_host_for_image(image: &str) -> String {
    // A single-segment ref (no '/' at all) is always an official Docker Hub
    // image ("busybox:latest") — its ':' is the tag separator, not a host
    // port, so it must never reach the "looks like a host" check below.
    let Some((first_segment, _rest)) = image.split_once('/') else {
        return "docker.io".to_string();
    };
    let looks_like_a_host = first_segment.contains('.') || first_segment.contains(':') || first_segment == "localhost";
    if looks_like_a_host {
        first_segment.to_string()
    } else {
        "docker.io".to_string()
    }
}


/// Extract `{username, password}` for `registry_host` out of a
/// `kubernetes.io/dockerconfigjson` Secret's `.dockerconfigjson` bytes
/// (`{"auths": {"<host>": {"username","password"} | {"auth": base64(u:p)}}}`).
/// Legacy `kubernetes.io/dockercfg` (no `"auths"` wrapper) isn't handled —
/// dockerconfigjson is what every current `kubectl create secret
/// docker-registry` produces.
pub(crate) fn parse_dockerconfigjson(bytes: &[u8], registry_host: &str) -> Option<(String, String)> {
    let json: serde_json::Value = serde_json::from_slice(bytes).ok()?;
    let auths = json.get("auths")?.as_object()?;

    // Docker Hub is recorded under several historical aliases.
    let candidates: Vec<&str> = if registry_host == "docker.io" {
        vec!["docker.io", "https://index.docker.io/v1/", "index.docker.io"]
    } else {
        vec![registry_host]
    };

    for key in candidates {
        let Some(entry) = auths.get(key) else { continue };
        if let (Some(u), Some(p)) =
            (entry.get("username").and_then(|v| v.as_str()), entry.get("password").and_then(|v| v.as_str()))
        {
            return Some((u.to_string(), p.to_string()));
        }
        if let Some(encoded) = entry.get("auth").and_then(|v| v.as_str()) {
            use base64::Engine;
            let decoded = base64::engine::general_purpose::STANDARD.decode(encoded).ok()?;
            let decoded = String::from_utf8(decoded).ok()?;
            let (u, p) = decoded.split_once(':')?;
            return Some((u.to_string(), p.to_string()));
        }
    }
    None
}


/// Fire a bare-minimum HTTP/1.1 GET for a `postStart`/`preStop` `httpGet`
/// lifecycle hook. Result is deliberately not inspected by the caller —
/// matches real kubelet, which only logs a failed lifecycle httpGet rather
/// than acting on it.
pub(crate) async fn lifecycle_http_get(host: &str, port: u16, path: &str) {
    if port == 0 {
        return;
    }
    let Ok(mut stream) = TcpStream::connect((host, port)).await else { return };
    let req = format!("GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n");
    let _ = stream.write_all(req.as_bytes()).await;
    let mut buf = [0u8; 256];
    let _ = stream.read(&mut buf).await;
}


impl CriRuntime {
    pub(crate) async fn list_pod_containers(&self, sandbox_id: &str) -> Result<Vec<v1::Container>> {
        let mut rt = self.rt.clone();
        let filter = ContainerFilter {
            pod_sandbox_id: sandbox_id.to_string(),
            ..Default::default()
        };
        let resp = rt
            .list_containers(ListContainersRequest { filter: Some(filter) })
            .await?
            .into_inner();
        Ok(resp.containers)
    }

    /// Find a container's CRI id within a sandbox by its `nodelet.dev/container-name` label.
    pub(crate) async fn find_container_id(&self, sandbox_id: &str, container_name: &str) -> Result<Option<String>> {
        let existing = self.list_pod_containers(sandbox_id).await?;
        Ok(existing
            .into_iter()
            .find(|c| c.labels.get(CTR_NAME_LABEL).map(|n| n == container_name).unwrap_or(false))
            .map(|c| c.id))
    }

    /// Resolve `{username, password}` for pulling `image` out of the given
    /// `imagePullSecrets` (by name, in the Pod's namespace) — the first
    /// secret with a matching registry host wins, same order kubelet itself
    /// tries them in. `None` if none match (or none are configured), in
    /// which case `PullImageRequest.auth` is left unset — fine for public
    /// images, the pre-existing behavior for everything until now.
    /// Mint a real, apiserver-signed ServiceAccount token via the
    /// `serviceaccounts/token` subresource (the `TokenRequest` API) — this
    /// is what backs a projected `serviceAccountToken` volume, the same
    /// mechanism every current `kube-api-access-*` volume uses on real
    /// Kubernetes. k8s-openapi 0.28 doesn't generate a typed helper for this
    /// subresource, so it's a raw POST via `kube::Client::request`.
    /// Needs nodelet's own client identity to have `create` on
    /// `serviceaccounts/token` in the target namespace — a real RBAC
    /// requirement, not a nodelet limitation; callers log and skip on
    /// failure rather than treating it as fatal to the whole pod.
    pub(crate) async fn resolve_service_account_token(
        &self,
        namespace: &str,
        service_account: &str,
        audiences: &[String],
        expiration_seconds: Option<i64>,
    ) -> Result<String> {
        use k8s_openapi::api::authentication::v1::{TokenRequest, TokenRequestSpec};
        let body = TokenRequest {
            metadata: Default::default(),
            spec: TokenRequestSpec {
                audiences: audiences.to_vec(),
                bound_object_ref: None,
                expiration_seconds,
            },
            status: None,
        };
        let bytes = serde_json::to_vec(&body).context("serializing TokenRequest")?;
        let req = http::Request::builder()
            .method("POST")
            .uri(format!("/api/v1/namespaces/{namespace}/serviceaccounts/{service_account}/token"))
            .header("Content-Type", "application/json")
            .body(bytes)
            .context("building TokenRequest HTTP request")?;
        let resp: TokenRequest = self.client.request(req).await.context("TokenRequest API call")?;
        resp.status.map(|s| s.token).context("TokenRequest response had no status.token")
    }

    /// `imagePullSecrets` are tried first (explicit, pod-declared intent);
    /// only if none of them resolve does this fall back to a configured
    /// image credential provider (round 71) — automatic discovery yields
    /// to an operator's own explicit configuration, not the other way
    /// around, since getting that backwards would be the more
    /// surprising direction to be wrong in.
    pub(crate) async fn resolve_pull_auth(&self, id: &PodId, pull_secrets: &[String], image: &str) -> Option<AuthConfig> {
        let registry_host = registry_host_for_image(image);
        for name in pull_secrets {
            let Ok(secret) = Api::<Secret>::namespaced(self.client.clone(), &id.namespace).get(name).await else {
                continue;
            };
            let Some(bytes) = secret.data.as_ref().and_then(|d| d.get(".dockerconfigjson")).map(|b| b.0.clone())
            else {
                continue;
            };
            if let Some((username, password)) = parse_dockerconfigjson(&bytes, &registry_host) {
                return Some(AuthConfig { username, password, ..Default::default() });
            }
        }
        self.resolve_credential_provider_auth(id, &registry_host, image).await
    }

    /// Round 71: image credential providers. `None` immediately if the
    /// feature isn't configured, or no provider's `matchImages` matches
    /// this image at all — the common case, costing nothing beyond one
    /// in-memory pattern check. A `tokenAttributes`-configured provider
    /// gets a real, audience-scoped `ServiceAccount` token (reusing the
    /// same `TokenRequest` machinery projected `serviceAccountToken`
    /// volumes already use) only after fetching the live `ServiceAccount`
    /// object and confirming `requireServiceAccount`/
    /// `requiredServiceAccountAnnotationKeys` are actually satisfied —
    /// this module's own `credential_provider.rs` deliberately doesn't
    /// touch the apiserver itself, keeping the exec-plugin protocol
    /// logic decoupled from nodelet's own kube client.
    async fn resolve_credential_provider_auth(&self, id: &PodId, registry_host: &str, image: &str) -> Option<AuthConfig> {
        let providers = self.credential_providers.as_ref()?;
        let provider = providers.first_match(image)?;

        let sa_ctx = if let Some(token_attrs) = &provider.token_attributes {
            match Api::<ServiceAccount>::namespaced(self.client.clone(), &id.namespace).get(&id.service_account_name).await {
                Ok(sa) => {
                    let sa_annotations = sa.metadata.annotations.clone().unwrap_or_default().into_iter().collect::<std::collections::BTreeMap<_, _>>();
                    let missing_required = token_attrs.required_service_account_annotation_keys.iter().any(|k| !sa_annotations.contains_key(k));
                    if missing_required {
                        warn!(provider = %provider.name, service_account = %id.service_account_name, "credential provider: ServiceAccount missing a required annotation key; skipping token mint");
                        None
                    } else {
                        let audiences = vec![token_attrs.service_account_token_audience.clone()];
                        match self.resolve_service_account_token(&id.namespace, &id.service_account_name, &audiences, None).await {
                            Ok(token) => Some(crate::credential_provider::ServiceAccountContext {
                                pod_name: id.name.clone(),
                                pod_namespace: id.namespace.clone(),
                                pod_uid: id.uid.clone(),
                                pod_annotations: Default::default(),
                                service_account_name: id.service_account_name.clone(),
                                service_account_uid: sa.metadata.uid.clone().unwrap_or_default(),
                                service_account_annotations: sa_annotations,
                                token,
                            }),
                            Err(e) => {
                                warn!(provider = %provider.name, error = ?e, "credential provider: failed to mint ServiceAccount token");
                                None
                            }
                        }
                    }
                }
                Err(e) => {
                    if token_attrs.require_service_account {
                        warn!(provider = %provider.name, service_account = %id.service_account_name, error = ?e, "credential provider: requireServiceAccount set but the ServiceAccount couldn't be fetched; skipping this provider");
                        return None;
                    }
                    None
                }
            }
        } else {
            None
        };

        providers.resolve(image, registry_host, sa_ctx.as_ref()).await
    }

    /// Pull `source.reference` (respecting the pod's `imagePullSecrets`,
    /// the same as any container image — `resolve_pull_auth()`) for a
    /// `volumeSource.image` volume (round 32, KEP-4639) and return the
    /// `ResolvedVolume::Image` CRI's own `Mount.image` field needs.
    /// `image_ref` comes from the runtime's own `PullImageResponse`, not
    /// the raw `source.reference` — matching the CRI proto's documented
    /// contract ("evaluates the returned `PullImageResponse.image_ref`
    /// value, which is then set to the `ImageSpec.image` field").
    pub(crate) async fn pull_image_volume(
        &self,
        id: &PodId,
        pull_secrets: &[String],
        source: &k8s_openapi::api::core::v1::ImageVolumeSource,
    ) -> Result<ResolvedVolume> {
        let reference = source.reference.clone().filter(|r| !r.is_empty()).context("image volume has no .reference set")?;
        let auth = self.resolve_pull_auth(id, pull_secrets, &reference).await;
        let mut img = self.img.clone();
        let resp = img
            .pull_image(PullImageRequest {
                image: Some(ImageSpec { image: reference, ..Default::default() }),
                auth,
                sandbox_config: None,
            })
            .await
            .context("PullImage for image volume")?
            .into_inner();
        Ok(ResolvedVolume::Image { image_ref: resp.image_ref })
    }

    /// Record which devices ended up backing a container, keyed the same
    /// way `restart_counts` is — so a later restart/removal can find and
    /// release them without re-deriving anything.
    pub(crate) fn record_device_allocations(&self, sandbox_id: &str, container_name: &str, allocations: Vec<(String, Vec<String>)>) {
        if allocations.is_empty() {
            return;
        }
        self.device_allocations.lock().unwrap().insert(restart_count_key(sandbox_id, container_name), allocations);
    }

    /// Give back every device allocation this list represents — used both
    /// when a just-attempted allocation needs to be unwound (container
    /// creation/start failed after devices were already picked) and as the
    /// shared tail end of `release_container_devices()`/
    /// `release_sandbox_devices()` below.
    pub(crate) fn release_devices(&self, allocations: &[(String, Vec<String>)]) {
        for (resource_name, device_ids) in allocations {
            self.device_plugins.release(resource_name, device_ids);
        }
    }

    /// Release and forget every device-plugin allocation *and* CPU Manager
    /// exclusive claim recorded for one container — call before recreating
    /// a container (restart-on-exit) or removing it outright, so both go
    /// back to their respective pools instead of being stranded as
    /// permanently "in use." Also drops the container from
    /// `container_resources` (it's gone, nothing left to refresh) and, if
    /// it held an exclusive CPU claim, sweeps the shared pool so its cores
    /// are actually usable by whatever's already running rather than just
    /// theoretically free.
    pub(crate) async fn release_container_devices(&self, sandbox_id: &str, container_name: &str) {
        let key = restart_count_key(sandbox_id, container_name);
        if let Some(allocations) = self.device_allocations.lock().unwrap().remove(&key) {
            self.release_devices(&allocations);
        }
        self.container_resources.lock().unwrap().remove(&key);
        self.applied_resources.lock().unwrap().remove(&key);
        self.spec_resources.lock().unwrap().remove(&key);
        if let Some(cpu_manager) = &self.cpu_manager {
            let was_exclusive = cpu_manager.is_exclusive(&key);
            cpu_manager.release(&key);
            if was_exclusive {
                self.refresh_shared_pool_cpusets().await;
            }
        }
        if let Some(memory_manager) = &self.memory_manager {
            memory_manager.release(&key);
        }
    }

    /// Same, for every container in a sandbox that's being torn down —
    /// mirrors `clear_restart_counts()`'s prefix-based sweep.
    pub(crate) async fn release_sandbox_devices(&self, sandbox_id: &str) {
        let prefix = format!("{sandbox_id}/");
        let removed: Vec<Vec<(String, Vec<String>)>> = {
            let mut table = self.device_allocations.lock().unwrap();
            let keys: Vec<String> = table.keys().filter(|k| k.starts_with(&prefix)).cloned().collect();
            keys.into_iter().filter_map(|k| table.remove(&k)).collect()
        };
        for allocations in removed {
            self.release_devices(&allocations);
        }
        self.container_resources.lock().unwrap().retain(|k, _| !k.starts_with(&prefix));
        self.applied_resources.lock().unwrap().retain(|k, _| !k.starts_with(&prefix));
        self.spec_resources.lock().unwrap().retain(|k, _| !k.starts_with(&prefix));
        if let Some(cpu_manager) = &self.cpu_manager {
            // Unconditionally refresh (unlike release_container_devices,
            // which only bothers when it knows a single container was
            // exclusive) — a sandbox can hold several containers, cheaper
            // to just always sweep once than track whether any of them
            // held a claim before release_sandbox() below forgets that.
            cpu_manager.release_sandbox(sandbox_id);
            self.refresh_shared_pool_cpusets().await;
        }
        if let Some(memory_manager) = &self.memory_manager {
            memory_manager.release_sandbox(sandbox_id);
        }
    }

    /// CPU Manager's retroactive half: bring every currently-tracked,
    /// non-exclusively-pinned container's `cpuset_cpus` in line with the
    /// current shared pool, via CRI's `UpdateContainerResources`. Called
    /// after any exclusive claim or release changes what the shared pool
    /// actually is. No-op if the policy is disabled. Best-effort per
    /// container — one runtime error updating a stale/gone container must
    /// not stop the rest from being refreshed; `container_resources` is
    /// only updated for entries that were actually applied successfully,
    /// so a failed update gets retried on the next pool change instead of
    /// nodelet believing it already happened.
    pub(crate) async fn refresh_shared_pool_cpusets(&self) {
        let Some(cpu_manager) = &self.cpu_manager else { return };
        let shared = crate::cpu_manager::format_cpuset(&cpu_manager.shared_pool());

        let entries: Vec<(String, String, LinuxContainerResources)> = self
            .container_resources
            .lock()
            .unwrap()
            .iter()
            .map(|(key, (container_id, resources))| (key.clone(), container_id.clone(), resources.clone()))
            .collect();

        let mut rt = self.rt.clone();
        for (key, container_id, mut resources) in entries {
            if cpu_manager.is_exclusive(&key) || resources.cpuset_cpus == shared {
                continue; // exclusively-pinned containers keep their own dedicated set; already-correct ones need no call
            }
            resources.cpuset_cpus = shared.clone();
            match rt
                .update_container_resources(UpdateContainerResourcesRequest {
                    container_id: container_id.clone(),
                    linux: Some(resources.clone()),
                    ..Default::default()
                })
                .await
            {
                Ok(_) => {
                    self.container_resources.lock().unwrap().insert(key, (container_id, resources));
                }
                Err(e) => {
                    warn!(container_id, error = ?e, "CPU Manager: failed to refresh a shared-pool container's cpuset; will retry on the next pool change");
                }
            }
        }
    }

    /// Run every currently-running app container's `preStop` hook (best
    /// effort — a failing hook must not block termination, matching real
    /// kubelet), then send each one `StopContainer` with the pod's
    /// termination grace period as the CRI timeout (containerd sends
    /// SIGTERM, waits up to that long, then SIGKILLs). Runs before
    /// `StopPodSandbox` so each container actually gets its own grace
    /// period instead of whatever the sandbox stop does by default.
    pub(crate) async fn graceful_stop_containers(&self, sandbox_id: &str, pod: &Pod, grace_seconds: i64) {
        let Ok(containers) = self.list_pod_containers(sandbox_id).await else { return };
        let running_v = ContainerState::ContainerRunning as i32;
        let pod_ip = self.pod_ip(sandbox_id).await.unwrap_or_default();
        // A still-*running* init-labeled container can only be a native
        // sidecar (round 36) — a regular init container blocks progression
        // until it exits, so it's never concurrently "running" alongside
        // this teardown path being reached. Sidecars get the same preStop +
        // graceful StopContainer treatment app containers do, so they're no
        // longer excluded here; their `preStop` hook (if any) lives on
        // `spec.initContainers`, not `spec.containers`, hence checking both.
        // **Simplification**: real kubelet stops sidecars strictly *after*
        // every app container has fully stopped; this stops everything in
        // one pass instead — not perfectly ordered, but every container
        // still gets its own graceful preStop + grace period.
        let spec_containers = pod.spec.as_ref().map(|s| s.containers.as_slice()).unwrap_or(&[]);
        let spec_init_containers = pod.spec.as_ref().and_then(|s| s.init_containers.as_deref()).unwrap_or(&[]);

        for c in &containers {
            if c.state != running_v {
                continue;
            }
            let Some(name) = c.labels.get(CTR_NAME_LABEL) else { continue };
            if let Some(pre_stop) = spec_containers
                .iter()
                .chain(spec_init_containers.iter())
                .find(|sc| &sc.name == name)
                .and_then(|sc| sc.lifecycle.as_ref())
                .and_then(|l| l.pre_stop.as_ref())
            {
                if let Err(e) = self.run_lifecycle_hook(&c.id, &pod_ip, pre_stop, grace_seconds).await {
                    warn!(container = %name, error = ?e, "preStop hook failed; continuing with termination anyway");
                }
            }
            let mut rt = self.rt.clone();
            let _ = rt.stop_container(StopContainerRequest { container_id: c.id.clone(), timeout: grace_seconds }).await;
        }
    }

    /// Execute one `postStart`/`preStop` handler. Supports `exec`, `httpGet`,
    /// and `sleep` (the newer preStop-only action) — not `tcpSocket` (the
    /// deprecated, rarely-used lifecycle hook form). Best-effort: errors are
    /// returned for the caller to log, never to block the container
    /// lifecycle transition that's waiting on this.
    pub(crate) async fn run_lifecycle_hook(
        &self,
        container_id: &str,
        pod_ip: &str,
        handler: &LifecycleHandler,
        timeout_secs: i64,
    ) -> Result<()> {
        let timeout = Duration::from_secs(timeout_secs.max(0) as u64);

        if let Some(exec) = &handler.exec {
            let command = exec.command.clone().unwrap_or_default();
            if command.is_empty() {
                return Ok(());
            }
            let mut rt = self.rt.clone();
            tokio::time::timeout(
                timeout,
                rt.exec_sync(ExecSyncRequest {
                    container_id: container_id.to_string(),
                    cmd: command,
                    timeout: timeout_secs.max(0),
                }),
            )
            .await
            .context("lifecycle hook exec timed out")?
            .context("lifecycle hook ExecSync")?;
            return Ok(());
        }

        if let Some(http) = &handler.http_get {
            let port = match &http.port {
                IntOrString::Int(n) => *n as u16,
                IntOrString::String(_) => 0, // named ports aren't resolvable here without the container spec; skip
            };
            let path = http.path.clone().unwrap_or_else(|| "/".to_string());
            let host = http.host.clone().filter(|h| !h.is_empty()).unwrap_or_else(|| pod_ip.to_string());
            // Best-effort — kubelet itself only logs a non-2xx/unreachable
            // lifecycle httpGet, it doesn't fail the container over it.
            let _ = tokio::time::timeout(timeout, lifecycle_http_get(&host, port, &path)).await;
            return Ok(());
        }

        if let Some(sleep) = &handler.sleep {
            tokio::time::sleep(Duration::from_secs(sleep.seconds.max(0) as u64).min(timeout)).await;
        }

        Ok(())
    }

    pub(crate) async fn container_exit_code(&self, container_id: &str) -> Result<i32> {
        Ok(self.container_status_details(container_id).await?.exit_code)
    }

    /// Full CRI `ContainerStatus` (exit code, reason, message, finished_at)
    /// for one container — the richer counterpart to `container_exit_code()`,
    /// used by `build_status()` (round 24) to populate `ContainerRuntimeStatus`'s
    /// terminated-state fields, not just decide pod phase.
    pub(crate) async fn container_status_details(&self, container_id: &str) -> Result<v1::ContainerStatus> {
        let mut rt = self.rt.clone();
        let resp = rt
            .container_status(ContainerStatusRequest { container_id: container_id.to_string(), verbose: false })
            .await
            .context("ContainerStatus")?
            .into_inner();
        resp.status.context("ContainerStatus response had no status")
    }

}
