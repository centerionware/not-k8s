use super::*;

/// A device-plugin allocation as checkpointed to disk, keyed by
/// (sandbox_id, container_name, resource_name) via
/// `device_alloc_checkpoint_path()`. Round 124 (found live in CI): before
/// this existed, a nodelet restart lost track of every already-allocated
/// device entirely — `DevicePlugins`' own `allocated`/`owners` maps and
/// `CriRuntime::device_allocations` are all purely in-memory. That's a
/// double bug: (1) a still-running container's already-in-use devices
/// would look free again the moment their plugin re-registers, so a
/// completely different new pod's allocation could double-book one of
/// them onto two containers at once, and (2) once that original
/// container was eventually torn down for real, `release_container_
/// devices()` would find nothing in `device_allocations` for it and
/// silently skip releasing anything at all — permanently stranding those
/// devices as "allocated" forever, the exact same shape of bug the CSI
/// mount-metadata sidecar (`csi.rs`'s `MountMeta`) already closed for
/// volumes. `pod_key` lets `DevicePlugins::restore_allocations_from_disk()` (called
/// from `register()` when a plugin reconnects) repopulate `owners` too,
/// not just `allocated`.
#[derive(serde::Serialize, serde::Deserialize)]
struct DeviceAllocMeta {
    container_name: String,
    resource_name: String,
    device_ids: Vec<String>,
    pod_key: String,
}

/// Where one (sandbox_id, container_name, resource_name) allocation is
/// checkpointed — see `DeviceAllocMeta`'s own doc comment for why this
/// exists at all. `container_name`/`resource_name` are also duplicated
/// *inside* the file itself (not just encoded in the filename) so reading
/// a matching file back never depends on reversing a lossy filename
/// encoding — every real device-plugin resource name is namespaced (e.g.
/// `nvidia.com/gpu`), so naively splitting the filename back apart on
/// `_` would be ambiguous the moment any name involved contains one.
fn device_alloc_checkpoint_path(sandbox_id: &str, container_name: &str, resource_name: &str) -> std::path::PathBuf {
    let safe_resource = resource_name.replace('/', "_");
    std::path::PathBuf::from(DEVICE_ALLOC_CHECKPOINT_DIR).join(format!("{sandbox_id}_{container_name}_{safe_resource}.json"))
}

/// Every allocation checkpointed under `sandbox_id` — used when the
/// in-memory `device_allocations` table doesn't have what's needed
/// (nodelet restarted since it was last populated). Filters by content,
/// not filename parsing (see `device_alloc_checkpoint_path()`'s own doc
/// comment for why), just a plain "does this sandbox_id's own directory
/// prefix match" filename check to avoid reading every unrelated pod's
/// checkpoints too.
fn read_device_alloc_checkpoints_for_sandbox(sandbox_id: &str) -> Vec<(std::path::PathBuf, DeviceAllocMeta)> {
    let prefix = format!("{sandbox_id}_");
    let Ok(entries) = std::fs::read_dir(DEVICE_ALLOC_CHECKPOINT_DIR) else { return Vec::new() };
    entries
        .flatten()
        .filter(|e| e.file_name().to_str().is_some_and(|n| n.starts_with(&prefix) && n.ends_with(".json")))
        .filter_map(|e| {
            let content = std::fs::read_to_string(e.path()).ok()?;
            let meta: DeviceAllocMeta = serde_json::from_str(&content).ok()?;
            Some((e.path(), meta))
        })
        .collect()
}

/// `DevicePlugins::health_of()`'s `Option<bool>` -> the
/// `ResourceHealth.health` API string (round 79) — matches upstream's
/// own 3 documented values exactly: a device plugin that deregistered
/// (or a device ID it no longer reports at all) reports `"Unknown"`
/// rather than being assumed either healthy or unhealthy.
pub(crate) fn resource_health_string(health: Option<bool>) -> &'static str {
    match health {
        Some(true) => "Healthy",
        Some(false) => "Unhealthy",
        None => "Unknown",
    }
}

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
    ///
    /// `bound_pod` (name, uid) sets `TokenRequestSpec.boundObjectRef` to the
    /// requesting Pod, matching what real kubelet always does for a
    /// projected `serviceAccountToken` volume — found missing live standing
    /// up a real DRA driver (round 121): the apiserver's
    /// `ServiceAccountTokenPodNodeInfo` enrichment (GA since 1.36, embeds
    /// `authentication.kubernetes.io/node-name` into the token's userInfo
    /// `extra`) only fires for tokens bound to a Pod with a resolvable
    /// `spec.nodeName` — an unbound token (this function's previous, only
    /// behavior) never gets it, so anything gating on that claim (the
    /// reference DRA driver's own `ValidatingAdmissionPolicy` on
    /// `ResourceSlice`, in this case) rejects every request. A real
    /// security property too, not just this one unblocking side effect:
    /// without it, a leaked projected token stays valid after its pod is
    /// deleted, unlike real kubelet's tokens. `None` for the credential-
    /// provider call site below, which mints a token before any pod exists.
    pub(crate) async fn resolve_service_account_token(
        &self,
        namespace: &str,
        service_account: &str,
        audiences: &[String],
        expiration_seconds: Option<i64>,
        bound_pod: Option<(&str, &str)>,
    ) -> Result<String> {
        use k8s_openapi::api::authentication::v1::{BoundObjectReference, TokenRequest, TokenRequestSpec};
        let bound_object_ref = bound_pod.map(|(name, uid)| BoundObjectReference {
            api_version: Some("v1".to_string()),
            kind: Some("Pod".to_string()),
            name: Some(name.to_string()),
            uid: Some(uid.to_string()),
        });
        let body = TokenRequest {
            metadata: Default::default(),
            spec: TokenRequestSpec {
                audiences: audiences.to_vec(),
                bound_object_ref,
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
                        // No pod exists yet at credential-provider time (this
                        // token is minted while resolving how to pull the
                        // image that will *become* the pod's container) —
                        // matches real kubelet, which doesn't bind these
                        // either.
                        match self.resolve_service_account_token(&id.namespace, &id.service_account_name, &audiences, None, None).await {
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
    /// release them without re-deriving anything. `pod_key` ("namespace/
    /// name") is also checkpointed to disk per resource (see
    /// `device_alloc_checkpoint_path()`'s own doc comment) — round 124,
    /// same "must survive a nodelet restart" reasoning as
    /// `CsiDrivers::mounted`'s on-disk `MountMeta` sidecar.
    pub(crate) fn record_device_allocations(&self, sandbox_id: &str, container_name: &str, pod_key: &str, allocations: Vec<(String, Vec<String>)>) {
        if allocations.is_empty() {
            return;
        }
        for (resource_name, device_ids) in &allocations {
            let meta = DeviceAllocMeta {
                container_name: container_name.to_string(),
                resource_name: resource_name.clone(),
                device_ids: device_ids.clone(),
                pod_key: pod_key.to_string(),
            };
            if let Ok(json) = serde_json::to_string(&meta) {
                let path = device_alloc_checkpoint_path(sandbox_id, container_name, resource_name);
                if let Some(parent) = path.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                if let Err(e) = std::fs::write(&path, json) {
                    warn!(sandbox_id, container = container_name, resource = %resource_name, error = ?e, "failed to checkpoint device allocation to disk — a nodelet restart before this container is torn down may leak these devices as permanently allocated");
                }
            }
        }
        self.device_allocations.lock().unwrap().insert(restart_count_key(sandbox_id, container_name), allocations);
    }

    /// `containerStatuses[].user.linux` (round 90) — a plain table read,
    /// no RPC; the actual fetch happens once at container-start time in
    /// `create_and_start_container()`.
    pub(crate) fn container_user_for(&self, sandbox_id: &str, container_name: &str) -> Option<(i64, i64, Vec<i64>)> {
        self.container_users.lock().unwrap().get(&restart_count_key(sandbox_id, container_name)).cloned()
    }

    /// `containerStatuses[].volumeMounts` (round 91) — a plain table
    /// read, no RPC; the actual computation happens once at
    /// container-creation time in `create_and_start_container()`.
    pub(crate) fn container_volume_mount_statuses_for(&self, sandbox_id: &str, container_name: &str) -> Vec<(String, String, bool, Option<String>)> {
        self.container_volume_mount_statuses.lock().unwrap().get(&restart_count_key(sandbox_id, container_name)).cloned().unwrap_or_default()
    }

    /// `containerStatuses[].allocatedResourcesStatus` (round 79;
    /// `ResourceHealthStatus`, found in round 72's re-audit) — live
    /// per-device health for every device-plugin allocation currently
    /// recorded for this container, queried straight off
    /// `device_plugins`' own `ListAndWatch`-fed state (no new tracking).
    /// Empty for a container with no device-plugin resources allocated
    /// at all.
    pub(crate) fn allocated_resources_status(&self, sandbox_id: &str, container_name: &str) -> Vec<(String, String, String)> {
        let key = restart_count_key(sandbox_id, container_name);
        let Some(allocations) = self.device_allocations.lock().unwrap().get(&key).cloned() else {
            return Vec::new();
        };
        allocations
            .into_iter()
            .flat_map(|(resource_name, device_ids)| {
                device_ids.into_iter().map(move |device_id| {
                    let health = resource_health_string(self.device_plugins.health_of(&resource_name, &device_id));
                    (resource_name.clone(), device_id, health.to_string())
                })
            })
            .collect()
    }

    /// Give back every device allocation this list represents — used both
    /// when a just-attempted allocation needs to be unwound (container
    /// creation/start failed after devices were already picked) and as the
    /// shared tail end of `release_container_devices()`/
    /// `release_sandbox_devices()` below. Also removes each allocation's
    /// on-disk checkpoint (see `DeviceAllocMeta`'s own doc comment) —
    /// best-effort (`remove_file` on a checkpoint that was never written,
    /// e.g. the just-attempted-allocation-unwind caller above, is a
    /// harmless no-op).
    pub(crate) fn release_devices(&self, sandbox_id: &str, container_name: &str, allocations: &[(String, Vec<String>)]) {
        for (resource_name, device_ids) in allocations {
            self.device_plugins.release(resource_name, device_ids);
            let _ = std::fs::remove_file(device_alloc_checkpoint_path(sandbox_id, container_name, resource_name));
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
        let allocations = self.device_allocations.lock().unwrap().remove(&key);
        // Round 124: the in-memory table is empty right after a nodelet
        // restart — fall back to whatever's checkpointed on disk for this
        // exact container, same "memory first, disk fallback" shape
        // `CsiDrivers::mounted_source_for()` already uses, so a pod
        // deleted after a restart still gets its devices released
        // instead of leaking them as permanently allocated forever.
        let allocations = allocations.unwrap_or_else(|| {
            read_device_alloc_checkpoints_for_sandbox(sandbox_id)
                .into_iter()
                .filter(|(_, meta)| meta.container_name == container_name)
                .map(|(_, meta)| (meta.resource_name, meta.device_ids))
                .collect()
        });
        if !allocations.is_empty() {
            self.release_devices(sandbox_id, container_name, &allocations);
        }
        self.container_resources.lock().unwrap().remove(&key);
        self.applied_resources.lock().unwrap().remove(&key);
        self.spec_resources.lock().unwrap().remove(&key);
        self.container_users.lock().unwrap().remove(&key);
        self.container_volume_mount_statuses.lock().unwrap().remove(&key);
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
        let mut by_container: HashMap<String, Vec<(String, Vec<String>)>> = {
            let mut table = self.device_allocations.lock().unwrap();
            let keys: Vec<String> = table.keys().filter(|k| k.starts_with(&prefix)).cloned().collect();
            keys.into_iter()
                .filter_map(|k| {
                    let container_name = k.strip_prefix(&prefix)?.to_string();
                    Some((container_name, table.remove(&k)?))
                })
                .collect()
        };
        // Round 124: same disk fallback as release_container_devices()
        // above, for the sandbox-wide teardown path — a nodelet restart
        // between a sandbox's containers being created and it later
        // being torn down would otherwise leak every one of their
        // devices as permanently allocated.
        for (_, meta) in read_device_alloc_checkpoints_for_sandbox(sandbox_id) {
            by_container.entry(meta.container_name.clone()).or_default().push((meta.resource_name, meta.device_ids));
        }
        for (container_name, allocations) in by_container {
            self.release_devices(sandbox_id, &container_name, &allocations);
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
