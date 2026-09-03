use super::*;

impl CriRuntime {
    /// Materialize every ConfigMap/Secret/emptyDir volume this Pod declares
    /// onto the host filesystem, and return volume name -> host directory.
    /// ConfigMap/Secret keys become individual files inside that directory
    /// (matching how a real kubelet lays them out, and how a Corefile-style
    /// single-key mount ends up as e.g. `.../Corefile`). Volume kinds this
    /// doesn't understand at all (`iscsi`, `nfs`, `fc`, and similar
    /// in-tree volume plugin types nodelet has no driver story for) are
    /// skipped with a warning rather than silently producing an empty
    /// mount — a container that needs one of those still won't get it,
    /// but at least it's visible in the logs why, instead of looking
    /// identical to the ConfigMap bug this fixes.
    pub(crate) async fn resolve_volumes(&self, pod: &Pod, id: &PodId, pull_secrets: &[String]) -> HashMap<String, ResolvedVolume> {
        let mut out = HashMap::new();
        let Some(volumes) = pod.spec.as_ref().and_then(|s| s.volumes.as_ref()) else {
            return out;
        };
        let pod_dir = PathBuf::from(VOLUME_ROOT).join(&id.uid).join("volumes");
        // `fsGroupChangePolicy` (round 93; found in round 92's re-audit)
        // is only ever honored by real kubelet for PersistentVolume-backed
        // (here: CSI-mounted) volumes — every other volume kind nodelet
        // materializes always gets the unconditional full chown, matching
        // upstream's own hardcoded-nil-policy behavior for those types.
        // `host_path_volume_names` tracks the opposite exclusion: a real
        // `hostPath` volume never gets fsGroup applied at all upstream
        // (no ownership-management support for that plugin), so it's
        // tracked here to keep it out of the fs_group loop below entirely.
        let mut csi_volume_names: HashSet<String> = HashSet::new();
        let mut host_path_volume_names: HashSet<String> = HashSet::new();

        for v in volumes {
            let vol_dir = pod_dir.join(&v.name);

            if let Some(cm) = &v.config_map {
                let name = &cm.name;
                let optional = cm.optional.unwrap_or(false);
                match Api::<ConfigMap>::namespaced(self.client.clone(), &id.namespace).get(name).await {
                    Ok(obj) => {
                        if let Err(e) = write_volume_dir(&vol_dir, obj.data, obj.binary_data.map(|m| {
                            m.into_iter().map(|(k, v)| (k, v.0)).collect()
                        })) {
                            warn!(volume = %v.name, configmap = %name, error = ?e, "failed to materialize ConfigMap volume");
                            continue;
                        }
                        out.insert(v.name.clone(), ResolvedVolume::HostPath(vol_dir));
                    }
                    // A missing ConfigMap on a volume explicitly marked
                    // `optional: true` (coredns's own manifest does this for
                    // its "coredns-custom" volume, for exactly this reason —
                    // it's fine for that ConfigMap to not exist) isn't a
                    // real problem; only warn for a genuinely required one.
                    Err(_) if optional => {}
                    Err(e) => warn!(volume = %v.name, configmap = %name, error = ?e, "failed to fetch ConfigMap for volume"),
                }
            } else if let Some(sec) = &v.secret {
                let Some(name) = &sec.secret_name else { continue };
                let optional = sec.optional.unwrap_or(false);
                match Api::<Secret>::namespaced(self.client.clone(), &id.namespace).get(name).await {
                    Ok(obj) => {
                        let bin = obj.data.map(|m| m.into_iter().map(|(k, v)| (k, v.0)).collect());
                        if let Err(e) = write_volume_dir(&vol_dir, obj.string_data, bin) {
                            warn!(volume = %v.name, secret = %name, error = ?e, "failed to materialize Secret volume");
                            continue;
                        }
                        out.insert(v.name.clone(), ResolvedVolume::HostPath(vol_dir));
                    }
                    Err(_) if optional => {}
                    Err(e) => warn!(volume = %v.name, secret = %name, error = ?e, "failed to fetch Secret for volume"),
                }
            } else if let Some(empty_dir) = &v.empty_dir {
                if let Err(e) = std::fs::create_dir_all(&vol_dir) {
                    warn!(volume = %v.name, error = ?e, "failed to create emptyDir volume");
                    continue;
                }
                if is_memory_medium_empty_dir(empty_dir) {
                    let size_limit_bytes = empty_dir.size_limit.as_ref().and_then(parse_memory_bytes);
                    if let Err(e) = mount_tmpfs_empty_dir(&vol_dir, size_limit_bytes) {
                        warn!(volume = %v.name, error = ?e, "failed to mount tmpfs for a Memory-medium emptyDir volume; falling back to a plain disk directory");
                    }
                } else if let Some(medium) = empty_dir.medium.as_deref().filter(|_| is_hugepages_medium_empty_dir(empty_dir)) {
                    let size_limit_bytes = empty_dir.size_limit.as_ref().and_then(parse_memory_bytes);
                    if let Err(e) = mount_hugetlbfs_empty_dir(&vol_dir, medium, size_limit_bytes) {
                        warn!(volume = %v.name, error = ?e, "failed to mount hugetlbfs for a HugePages-medium emptyDir volume; falling back to a plain disk directory");
                    }
                }
                out.insert(v.name.clone(), ResolvedVolume::HostPath(vol_dir));
            } else if let Some(downward) = &v.downward_api {
                if let Err(e) = write_downward_api_volume(&vol_dir, pod, downward.items.as_deref().unwrap_or(&[])) {
                    warn!(volume = %v.name, error = ?e, "failed to materialize downwardAPI volume");
                    continue;
                }
                out.insert(v.name.clone(), ResolvedVolume::HostPath(vol_dir));
            } else if let Some(projected) = &v.projected {
                if let Err(e) = self.write_projected_volume(&vol_dir, pod, id, projected).await {
                    warn!(volume = %v.name, error = ?e, "failed to materialize projected volume");
                    continue;
                }
                out.insert(v.name.clone(), ResolvedVolume::HostPath(vol_dir));
            } else if let Some(pvc_source) = &v.persistent_volume_claim {
                match self.resolve_csi_source(&id.namespace, &pvc_source.claim_name).await {
                    Ok(Some(mut source)) => {
                        source.read_only |= pvc_source.read_only.unwrap_or(false);
                        let block = source.block;
                        match self.csi.mount(&source, &vol_dir, &id.uid, false).await {
                            Ok(()) => {
                                // Raw block volumes (round 77): the exact
                                // same host path mount() just published to
                                // -- for Block mode it's a device-node
                                // bind-mount FILE, not a directory, so it's
                                // resolved to BlockDevice instead of
                                // HostPath (build_devices() picks it up for
                                // volumeDevices; build_mounts() explicitly
                                // ignores this variant).
                                let resolved = if block { ResolvedVolume::BlockDevice(vol_dir) } else { ResolvedVolume::HostPath(vol_dir) };
                                out.insert(v.name.clone(), resolved);
                                csi_volume_names.insert(v.name.clone());
                            }
                            Err(e) => warn!(volume = %v.name, claim = %pvc_source.claim_name, error = ?e, "failed to mount CSI volume"),
                        }
                    }
                    Ok(None) => {
                        // Not yet Bound, no CSI source, or no driver
                        // configured for it — resolve_csi_source() already
                        // warned with the specific reason. Same as any
                        // other unresolvable volume: silently absent from
                        // the mount map, container starts without it.
                    }
                    Err(e) => warn!(volume = %v.name, claim = %pvc_source.claim_name, error = ?e, "failed to resolve PersistentVolumeClaim"),
                }
            } else if v.ephemeral.is_some() {
                // Generic ephemeral volume (round 31): the actual PVC is
                // created by the ephemeral-volume controller (a
                // kube-controller-manager component), not nodelet — same
                // "not kubelet's job" boundary as dynamic provisioning
                // elsewhere in this file. Once that controller has created
                // it, it behaves exactly like a normal PVC reference, so
                // this reuses resolve_csi_source() for everything past the
                // ownership safety check.
                let claim_name = ephemeral_pvc_name(&id.name, &v.name);
                match self.resolve_ephemeral_source(&id.namespace, &claim_name, &id.uid).await {
                    Ok(Some(source)) => {
                        let block = source.block;
                        match self.csi.mount(&source, &vol_dir, &id.uid, false).await {
                            Ok(()) => {
                                let resolved = if block { ResolvedVolume::BlockDevice(vol_dir) } else { ResolvedVolume::HostPath(vol_dir) };
                                out.insert(v.name.clone(), resolved);
                                csi_volume_names.insert(v.name.clone());
                            }
                            Err(e) => warn!(volume = %v.name, claim = %claim_name, error = ?e, "failed to mount CSI volume for generic ephemeral volume"),
                        }
                    }
                    Ok(None) => {
                        // Not yet created by the ephemeral-volume
                        // controller, not owned by this pod, or otherwise
                        // unresolvable — resolve_ephemeral_source() already
                        // warned with the specific reason.
                    }
                    Err(e) => warn!(volume = %v.name, claim = %claim_name, error = ?e, "failed to resolve generic ephemeral volume's PersistentVolumeClaim"),
                }
            } else if let Some(csi_source) = &v.csi {
                // CSI ephemeral (inline) volume (round 46; found in round
                // 45's re-audit) — no PV/PVC at all, just this volume's own
                // CSIVolumeSource fields (e.g. secrets-store-csi-driver's
                // "mount a Secret from Vault directly" pattern).
                match self.resolve_csi_ephemeral_source(&id.namespace, csi_source, &id.uid, &v.name).await {
                    Some(source) => match self.csi.mount(&source, &vol_dir, &id.uid, true).await {
                        Ok(()) => {
                            out.insert(v.name.clone(), ResolvedVolume::HostPath(vol_dir));
                            csi_volume_names.insert(v.name.clone());
                        }
                        Err(e) => warn!(volume = %v.name, driver = %csi_source.driver, error = ?e, "failed to mount CSI ephemeral volume"),
                    },
                    None => {
                        // No driver configured — resolve_csi_ephemeral_source()
                        // already warned with the specific reason.
                    }
                }
            } else if let Some(image_source) = &v.image {
                // volumeSource.image (round 32, KEP-4639): CRI has direct
                // native support for this via Mount.image — kubelet's own
                // job is just to PullImage the reference (respecting the
                // pod's imagePullSecrets, same as any container image) and
                // pass the runtime's resolved image_ref through; the
                // runtime does the actual mounting.
                match self.pull_image_volume(id, pull_secrets, image_source).await {
                    Ok(resolved) => {
                        out.insert(v.name.clone(), resolved);
                    }
                    Err(e) => warn!(volume = %v.name, reference = %image_source.reference.as_deref().unwrap_or(""), error = ?e, "failed to pull image for image volume"),
                }
            } else if let Some(hp) = &v.host_path {
                // hostPath (round 65; found in a fresh gap re-audit): unlike
                // every other volume kind above, this isn't materialized
                // under nodelet's own VOLUME_ROOT — it's the host's own
                // existing path, used directly, exactly matching upstream's
                // "no ownership, no cleanup on pod deletion" semantics for
                // this volume type.
                let path = PathBuf::from(&hp.path);
                match validate_host_path(&path, hp.type_.as_deref()) {
                    Ok(()) => {
                        out.insert(v.name.clone(), ResolvedVolume::HostPath(path));
                        // fsGroup (round 93; found in round 92's re-audit,
                        // verified against upstream before implementing):
                        // real kubelet's hostPath plugin doesn't support
                        // ownership management at all -- fsGroup is never
                        // applied to a hostPath volume, since it's the
                        // host's own pre-existing directory, not something
                        // the pod owns. Tracked here so the fs_group loop
                        // below excludes it entirely.
                        host_path_volume_names.insert(v.name.clone());
                    }
                    Err(e) => {
                        warn!(volume = %v.name, path = %hp.path, host_path_type = %hp.type_.as_deref().unwrap_or(""), error = %e, "hostPath volume failed validation");
                        out.insert(v.name.clone(), ResolvedVolume::Invalid(e));
                    }
                }
            } else {
                warn!(
                    volume = %v.name,
                    volume_type = volume_source_type(v),
                    pod = %format!("{}/{}", id.namespace, id.name),
                    "volume type not supported yet (configMap/secret/emptyDir/downwardAPI/projected are) — \
                     any container mounting it won't get this path");
            }
        }

        if let Some(aliases) = pod.spec.as_ref().and_then(|s| s.host_aliases.as_ref()).filter(|a| !a.is_empty()) {
            let hosts_path = pod_dir.join("etc-hosts");
            match write_etc_hosts(&hosts_path, aliases) {
                Ok(()) => {
                    out.insert(ETC_HOSTS_VOLUME_KEY.to_string(), ResolvedVolume::HostPath(hosts_path));
                }
                Err(e) => warn!(error = ?e, "failed to materialize /etc/hosts for hostAliases"),
            }
        }

        let pod_sc = pod.spec.as_ref().and_then(|s| s.security_context.as_ref());
        if let Some(fs_group) = pod_sc.and_then(|sc| sc.fs_group) {
            let fs_group = fs_group as u32;
            // `fsGroupChangePolicy` (round 93; found in round 92's
            // re-audit) is only ever honored upstream for PV-backed
            // (here: CSI) volumes — see `requires_fs_group_change()`'s
            // doc comment.
            let fs_group_change_policy = pod_sc.and_then(|sc| sc.fs_group_change_policy.as_deref());
            for (key, source) in &out {
                if key == ETC_HOSTS_VOLUME_KEY {
                    continue; // a single file, not a directory nodelet materialized as a tree
                }
                // hostPath volumes never get fsGroup applied at all,
                // matching upstream — see where `host_path_volume_names`
                // is populated above.
                if host_path_volume_names.contains(key) {
                    continue;
                }
                // Image volumes (round 32) are read-only OCI content with
                // no host directory of nodelet's own to chown at all —
                // fsGroup doesn't apply to them, matching upstream.
                let ResolvedVolume::HostPath(dir) = source else { continue };
                if csi_volume_names.contains(key) && skip_fs_group_change(dir, fs_group, fs_group_change_policy) {
                    debug!(dir = %dir.display(), fs_group, "skipping fsGroup recursive chown; root already matches (fsGroupChangePolicy: OnRootMismatch)");
                    continue;
                }
                if let Err(e) = apply_fs_group(dir, fs_group) {
                    warn!(dir = %dir.display(), fs_group, error = ?e, "failed to apply fsGroup to volume");
                }
            }
        }

        // `hostUsers: false` (round 25) needs every volume nodelet itself
        // materialized (not a real hostPath — see `host_path_volume_names`
        // above, same exclusion `apply_fs_group()` uses and for the same
        // reason: that's the host's own pre-existing directory, not the
        // pod's to chown) actually owned on-disk by this pod's allocated
        // userns range base, or the sandbox's own ambient namespace
        // (`sandbox_config()`'s `UserNamespace` mapping, round 25) has
        // nothing real to translate — see `chown_userns_base()`'s doc
        // comment for the full mechanism, and why round 123 removed the
        // per-mount idmapping round 88 used to layer on top of this.
        // Independent of `fsGroup` above: this is needed even when no
        // fsGroup is set at all, since it's ownership for the container's
        // own (mapped) root user, not group-shared access.
        if !id.host_users {
            if let Some((host_base, _length)) = self.userns.assigned(&id.uid) {
                for (key, source) in &out {
                    if host_path_volume_names.contains(key) {
                        continue;
                    }
                    let ResolvedVolume::HostPath(dir) = source else { continue };
                    if let Err(e) = chown_userns_base(dir, host_base) {
                        warn!(dir = %dir.display(), host_base, error = ?e, "failed to chown volume to the pod's userns base uid/gid");
                    }
                }
            }
        }

        out
    }

    /// Materialize a `projected` volume: each source contributes files into
    /// the same directory (real Kubernetes semantics — sources are merged,
    /// not nested). `serviceAccountToken` is implemented via the real
    /// `TokenRequest` API, bound to the requesting Pod (see
    /// `resolve_service_account_token()`'s own doc comment). `clusterTrustBundle`
    /// isn't — skipped with a warning, same treatment as any other
    /// unsupported volume type.
    pub(crate) async fn write_projected_volume(
        &self,
        dir: &std::path::Path,
        pod: &Pod,
        id: &PodId,
        projected: &k8s_openapi::api::core::v1::ProjectedVolumeSource,
    ) -> Result<()> {
        for source in projected.sources.as_deref().unwrap_or(&[]) {
            if let Some(cm) = &source.config_map {
                let optional = cm.optional.unwrap_or(false);
                match Api::<ConfigMap>::namespaced(self.client.clone(), &id.namespace).get(&cm.name).await {
                    Ok(obj) => {
                        let bin = obj.binary_data.map(|m| m.into_iter().map(|(k, v)| (k, v.0)).collect());
                        write_projected_keys(dir, obj.data, bin, cm.items.as_deref())?;
                    }
                    Err(_) if optional => {}
                    Err(e) => warn!(configmap = %cm.name, error = ?e, "projected volume: failed to fetch ConfigMap source"),
                }
            } else if let Some(sec) = &source.secret {
                let optional = sec.optional.unwrap_or(false);
                match Api::<Secret>::namespaced(self.client.clone(), &id.namespace).get(&sec.name).await {
                    Ok(obj) => {
                        let bin = obj.data.map(|m| m.into_iter().map(|(k, v)| (k, v.0)).collect());
                        write_projected_keys(dir, obj.string_data, bin, sec.items.as_deref())?;
                    }
                    Err(_) if optional => {}
                    Err(e) => warn!(secret = %sec.name, error = ?e, "projected volume: failed to fetch Secret source"),
                }
            } else if let Some(da) = &source.downward_api {
                write_downward_api_volume(dir, pod, da.items.as_deref().unwrap_or(&[]))?;
            } else if let Some(sat) = &source.service_account_token {
                let target = dir.join(&sat.path);
                // Issue #554: this used to call TokenRequest unconditionally
                // on every single materialization -- including a routine
                // reconcile that's only reusing an already-running sandbox,
                // not creating anything new. Real kubelet caches a projected
                // token and only refreshes it near actual expiry (~80% of
                // its TTL); a pod that reconciles often for an unrelated
                // reason (a flapping liveness probe, live-observed this
                // session restarting one every 30s for hours) otherwise pays
                // a full TokenRequest round trip -- real nodeapiserver
                // admission/authn/authz work and a real nodestore write --
                // on every reconcile, not just when the token actually needs
                // refreshing.
                if !token_needs_refresh(std::fs::metadata(&target).and_then(|m| m.modified()).ok(), std::time::SystemTime::now(), sat.expiration_seconds) {
                    continue;
                }
                let service_account = pod_service_account_name(pod);
                let audiences = token_audiences(sat.audience.as_deref());
                match self
                    .resolve_service_account_token(&id.namespace, &service_account, &audiences, sat.expiration_seconds, Some((&id.name, &id.uid)))
                    .await
                {
                    Ok(token) => {
                        if let Some(parent) = target.parent() {
                            std::fs::create_dir_all(parent)?;
                        }
                        std::fs::write(target, token)?;
                    }
                    Err(e) => warn!(
                        pod = %format!("{}/{}", id.namespace, id.name),
                        service_account, error = ?e,
                        "projected volume: serviceAccountToken TokenRequest failed (RBAC needs `create` on serviceaccounts/token)"
                    ),
                }
            } else if source.cluster_trust_bundle.is_some() {
                warn!(
                    pod = %format!("{}/{}", id.namespace, id.name),
                    "projected volume: clusterTrustBundle source not supported"
                );
            }
        }
        Ok(())
    }

    /// Resolve a `spec.volumes[].ephemeral` (generic ephemeral) volume to
    /// its CSI source (round 31). Real kubelet doesn't create the backing
    /// PVC itself — that's the ephemeral-volume controller's job (a
    /// kube-controller-manager component), same "not kubelet's job"
    /// boundary as dynamic provisioning elsewhere in this file — so this
    /// only ever *reads* whatever that controller has already created at
    /// the deterministic name `ephemeral_pvc_name()` computes.
    ///
    /// Unlike `resolve_csi_source()` (used for a direct
    /// `persistentVolumeClaim` reference, where a missing PVC usually
    /// means a typo/misconfiguration worth surfacing as an error), a
    /// missing PVC here is the *expected*, normal state immediately after
    /// pod creation — the controller hasn't gotten to it yet — so this
    /// checks existence itself first and treats "doesn't exist yet" as a
    /// graceful `Ok(None)` retry, not a warning-level error.
    ///
    /// Also does the safety check `EphemeralVolumeSource`'s own API doc
    /// comment describes: a same-named PVC that isn't actually owned by
    /// this pod (checked by UID) is never used, even if bound and
    /// otherwise valid — avoids adopting an unrelated volume by mistake
    /// (e.g. a naming collision, or a leftover PVC from a previous pod).
    pub(crate) async fn resolve_ephemeral_source(
        &self,
        namespace: &str,
        claim_name: &str,
        pod_uid: &str,
    ) -> Result<Option<crate::runtime::csi::CsiVolumeSource>> {
        let pvc = match Api::<PersistentVolumeClaim>::namespaced(self.client.clone(), namespace).get_opt(claim_name).await {
            Ok(Some(pvc)) => pvc,
            Ok(None) => {
                warn!(claim = %claim_name, "generic ephemeral volume: PersistentVolumeClaim doesn't exist yet — waiting for the ephemeral-volume controller to create it; will retry next reconcile");
                return Ok(None);
            }
            Err(e) => return Err(e).with_context(|| format!("fetching PersistentVolumeClaim {claim_name}")),
        };
        if !pvc_owned_by_pod(&pvc, pod_uid) {
            warn!(claim = %claim_name, "generic ephemeral volume: a PersistentVolumeClaim with the expected name exists but isn't owned by this pod; refusing to use it (matches real kubelet's own safety check)");
            return Ok(None);
        }
        self.resolve_csi_source(namespace, claim_name).await
    }

    /// Resolve a `PersistentVolumeClaim` (by name, in `namespace`) to its
    /// bound `PersistentVolume`'s CSI source — `Ok(None)` (not an error) for
    /// every legitimate "nothing to mount yet" case: the PVC doesn't exist,
    /// isn't Bound yet (`.spec.volumeName` unset — this is normal right
    /// after pod creation if a provisioner is still creating the PV; the
    /// next reconcile tries again), the bound PV isn't backed by CSI at all
    /// (an in-tree volume type — out of scope for this slice), or no CSI
    /// driver is configured in `NODELET_CSI_DRIVERS` for it. Each case is
    /// logged with its specific reason so "why isn't my volume mounted"
    /// doesn't require reading source to answer.
    pub(crate) async fn resolve_csi_source(&self, namespace: &str, claim_name: &str) -> Result<Option<crate::runtime::csi::CsiVolumeSource>> {
        let pvc = match Api::<PersistentVolumeClaim>::namespaced(self.client.clone(), namespace).get(claim_name).await {
            Ok(pvc) => pvc,
            Err(e) => return Err(e).with_context(|| format!("fetching PersistentVolumeClaim {claim_name}")),
        };
        let Some(pv_name) = pvc.spec.as_ref().and_then(|s| s.volume_name.as_ref()) else {
            warn!(claim = %claim_name, "PersistentVolumeClaim not yet Bound to a PersistentVolume; will retry next reconcile");
            return Ok(None);
        };

        let pv = match Api::<PersistentVolume>::all(self.client.clone()).get(pv_name).await {
            Ok(pv) => pv,
            Err(e) => return Err(e).with_context(|| format!("fetching PersistentVolume {pv_name}")),
        };
        let Some(csi) = pv.spec.as_ref().and_then(|s| s.csi.as_ref()) else {
            warn!(claim = %claim_name, volume = %pv_name, "bound PersistentVolume has no .spec.csi source (an in-tree volume type isn't supported)");
            return Ok(None);
        };

        if !self.csi.driver_configured(&csi.driver) {
            warn!(claim = %claim_name, driver = %csi.driver, "no CSI driver configured for this PersistentVolume's driver — set NODELET_CSI_DRIVERS");
            return Ok(None);
        }

        let publish_context = if self.driver_requires_attach(&csi.driver).await {
            let attachments = Api::<VolumeAttachment>::all(self.client.clone())
                .list(&ListParams::default())
                .await
                .context("listing VolumeAttachments")?;
            match find_volume_attachment(&attachments.items, &csi.driver, &self.node_name, pv_name) {
                Some(att) => match attachment_publish_context(att) {
                    Some(ctx) => ctx,
                    None => {
                        warn!(claim = %claim_name, volume = %pv_name, driver = %csi.driver, "VolumeAttachment found but not yet attached; will retry next reconcile");
                        return Ok(None);
                    }
                },
                None => {
                    warn!(claim = %claim_name, volume = %pv_name, driver = %csi.driver, "driver requires attach but no matching VolumeAttachment exists yet (external-attacher hasn't created it); will retry next reconcile");
                    return Ok(None);
                }
            }
        } else {
            HashMap::new()
        };

        let node_stage_secrets = self.resolve_csi_secret_ref(csi.node_stage_secret_ref.as_ref(), namespace).await;
        let node_publish_secrets = self.resolve_csi_secret_ref(csi.node_publish_secret_ref.as_ref(), namespace).await;

        // Raw block volumes (round 77; found in round 76's re-audit):
        // PersistentVolume.spec.volumeMode == "Block" (default
        // "Filesystem" when unset, matching the API's own default).
        let block = pv.spec.as_ref().and_then(|s| s.volume_mode.as_deref()) == Some("Block");

        Ok(Some(crate::runtime::csi::CsiVolumeSource {
            driver: csi.driver.clone(),
            volume_handle: csi.volume_handle.clone(),
            fs_type: csi.fs_type.clone().unwrap_or_default(),
            read_only: csi.read_only.unwrap_or(false),
            volume_attributes: csi.volume_attributes.clone().unwrap_or_default().into_iter().collect(),
            node_stage_secrets,
            node_publish_secrets,
            publish_context,
            block,
        }))
    }

    /// Whether `driver` needs an attach before it can be staged/published —
    /// `CSIDriver.spec.attachRequired`, defaulting to "yes" (matching
    /// upstream: a driver with no `CSIDriver` object registered at all, or
    /// one that doesn't set the field, is assumed to require attach). Most
    /// node-local/edge storage drivers explicitly set `attachRequired:
    /// false` and skip this path entirely — this only matters for drivers
    /// backed by real block storage (cloud disks, SANs, ...).
    pub(crate) async fn driver_requires_attach(&self, driver: &str) -> bool {
        match Api::<CSIDriver>::all(self.client.clone()).get(driver).await {
            Ok(obj) => attach_required(Some(&obj)),
            Err(_) => attach_required(None),
        }
    }

    /// Resolve a CSI `SecretReference` (`nodeStageSecretRef`/
    /// `nodePublishSecretRef`) to key/value pairs for the CSI request's
    /// `secrets` map. Empty (not an error) if `reference` is `None` — most
    /// drivers don't need one at all. `SecretReference.namespace` is
    /// itself optional (PVs are cluster-scoped, so unlike every other
    /// Secret reference in this file there's no natural pod namespace to
    /// default to) — falls back to `default_namespace` (the PVC's own
    /// namespace) when unset, matching what most CSI driver docs assume.
    pub(crate) async fn resolve_csi_secret_ref(&self, reference: Option<&SecretReference>, default_namespace: &str) -> HashMap<String, String> {
        let Some(reference) = reference else { return HashMap::new() };
        let Some(name) = reference.name.as_deref() else { return HashMap::new() };
        let namespace = reference.namespace.as_deref().unwrap_or(default_namespace);
        match Api::<Secret>::namespaced(self.client.clone(), namespace).get(name).await {
            Ok(secret) => secret
                .data
                .unwrap_or_default()
                .into_iter()
                .map(|(k, v)| (k, String::from_utf8_lossy(&v.0).into_owned()))
                .collect(),
            Err(e) => {
                warn!(secret = %name, namespace, error = ?e, "CSI: failed to fetch a nodeStageSecretRef/nodePublishSecretRef Secret; proceeding without it");
                HashMap::new()
            }
        }
    }

    /// Resolve a CSI *ephemeral inline* volume (`volumes[].csi` specified
    /// directly — round 46; found in round 45's re-audit) — distinct from
    /// both the PVC path (`resolve_csi_source()`) and the generic
    /// `ephemeral` (PVC-templated) path (round 31): there's no PV/PVC
    /// object at all here, just the volume's own `CSIVolumeSource` fields.
    /// Real-world drivers like `secrets-store-csi-driver` use this form to
    /// mount secrets from an external store with no PVC involved.
    pub(crate) async fn resolve_csi_ephemeral_source(
        &self,
        namespace: &str,
        csi: &k8s_openapi::api::core::v1::CSIVolumeSource,
        pod_uid: &str,
        volume_name: &str,
    ) -> Option<crate::runtime::csi::CsiVolumeSource> {
        if !self.csi.driver_configured(&csi.driver) {
            warn!(driver = %csi.driver, volume = %volume_name, "CSI ephemeral volume: no CSI driver configured — set NODELET_CSI_DRIVERS or wait for it to register");
            return None;
        }
        let node_publish_secrets = match &csi.node_publish_secret_ref {
            Some(local_ref) => {
                let secret_ref = SecretReference { name: Some(local_ref.name.clone()), namespace: None };
                self.resolve_csi_secret_ref(Some(&secret_ref), namespace).await
            }
            None => HashMap::new(),
        };
        let mut volume_attributes: HashMap<String, String> = csi
            .volume_attributes
            .clone()
            .unwrap_or_default()
            .into_iter()
            .collect();
        // Kubernetes marks direct `volumes[].csi` publishes explicitly. A
        // driver such as the reference hostpath driver uses this bit to
        // create the synthetic volume on NodePublishVolume; without it, the
        // deterministic handle below is treated as a pre-created volume ID
        // and the inline mount fails with NotFound.
        volume_attributes.insert(
            "csi.storage.k8s.io/ephemeral".to_string(),
            "true".to_string(),
        );

        Some(crate::runtime::csi::CsiVolumeSource {
            driver: csi.driver.clone(),
            volume_handle: csi_ephemeral_volume_handle(pod_uid, volume_name),
            fs_type: csi.fs_type.clone().unwrap_or_default(),
            read_only: csi.read_only.unwrap_or(false),
            volume_attributes,
            // Ephemeral inline volumes never stage (no NodeStageVolume) and
            // have no attach concept (no VolumeAttachment) — see
            // `CsiDrivers::mount()`'s `ephemeral` parameter.
            node_stage_secrets: HashMap::new(),
            node_publish_secrets,
            publish_context: HashMap::new(),
            // CSI ephemeral inline volumes have no `volumeMode` concept in
            // the API at all — always Filesystem-shaped.
            block: false,
        })
    }

    /// Unpublish (and, if this was the last pod using it, unstage) every
    /// CSI-backed `PersistentVolumeClaim` volume this pod referenced.
    /// Best-effort per volume — one failing CSI driver call must not stop
    /// the rest of teardown, same treatment `graceful_stop_containers`
    /// already gives a failing `preStop` hook. Re-resolves the PVC->PV
    /// chain rather than remembering it from `ensure_pod()` time: simpler
    /// than a second side table, at the cost of a volume whose PVC/PV was
    /// deleted out from under a still-running pod not getting cleanly
    /// unmounted (logged, not silently lost — a real but narrow gap).
    pub(crate) async fn unmount_csi_volumes(&self, pod: &Pod, id: &PodId) {
        let Some(volumes) = pod.spec.as_ref().and_then(|s| s.volumes.as_ref()) else { return };
        let pod_dir = PathBuf::from(VOLUME_ROOT).join(&id.uid).join("volumes");

        for v in volumes {
            if let Some(csi_source) = &v.csi {
                // CSI ephemeral inline volume (round 46) — no PVC to
                // resolve; re-derive the same synthetic volume_handle
                // resolve_volumes() minted at mount time.
                let vol_dir = pod_dir.join(&v.name);
                let volume_handle = csi_ephemeral_volume_handle(&id.uid, &v.name);
                if let Err(e) = self.csi.unmount(&csi_source.driver, &volume_handle, &vol_dir, &id.uid, true).await {
                    warn!(volume = %v.name, driver = %csi_source.driver, error = ?e, "CSI teardown: failed to unmount ephemeral inline volume");
                }
                continue;
            }
            let claim_name = if let Some(pvc_source) = &v.persistent_volume_claim {
                pvc_source.claim_name.clone()
            } else if v.ephemeral.is_some() {
                // Generic ephemeral volume (round 31) — same deterministic
                // name ensure_pod()'s resolve_volumes() derives it by.
                ephemeral_pvc_name(&id.name, &v.name)
            } else {
                continue;
            };
            let vol_dir = pod_dir.join(&v.name);
            // Round 124 (found live in CI): prefer what `mount()` already
            // recorded (CsiDrivers::mounted, backed by an on-disk sidecar
            // so it survives a nodelet restart too — keyed by this exact
            // target_path) over re-fetching the PVC — the PVC is routinely
            // gone by teardown (pod deleted, then its PVC, is completely
            // ordinary cleanup) and re-resolving via a live API call used
            // to permanently abandon the volume the instant that fetch
            // 404'd. Only falls back to the PVC-based resolution below when
            // nothing was recorded at all (this volume predates round 124,
            // or the sidecar write itself failed at mount time) — see
            // `mounted`'s own doc comment in csi.rs for the full story.
            let (driver, volume_handle) = if let Some(cached) = self.csi.mounted_source_for(&vol_dir) {
                cached
            } else {
                match self.resolve_csi_source(&id.namespace, &claim_name).await {
                    Ok(Some(source)) => (source.driver, source.volume_handle),
                    Ok(None) => continue, // already logged why in resolve_csi_source()
                    Err(e) => {
                        warn!(volume = %v.name, claim = %claim_name, error = ?e, "CSI teardown: failed to resolve PersistentVolumeClaim; volume left mounted");
                        continue;
                    }
                }
            };
            if let Err(e) = self.csi.unmount(&driver, &volume_handle, &vol_dir, &id.uid, false).await {
                warn!(volume = %v.name, driver = %driver, error = ?e, "CSI teardown: failed to unmount volume");
            }
        }
    }

}
