use super::*;

/// Dynamic Resource Allocation (round 63): the CDI device IDs a pod-claim
/// resolved to, split by which `DeviceRequest` name (within the claim)
/// produced them so `container.resources.claims[].request` can filter to
/// just one, alongside the flattened union (`all`) for the common case of
/// a container referencing the whole claim.
#[derive(Default, Clone, Debug)]
pub(crate) struct PreparedPodClaim {
    pub(crate) by_request: HashMap<String, Vec<String>>,
    pub(crate) all: Vec<String>,
}


/// `spec.resourceClaims[].name` -> the actual `ResourceClaim` object name
/// to fetch. Direct (`resourceClaimName` set) is a pure pass-through;
/// template-based (`resourceClaimTemplateName` set instead) requires
/// looking up the generated name the resource-claim controller recorded
/// in `pod.status.resourceClaimStatuses` (keyed by the pod-claim's own
/// `name`, not by template name) — `None` if that hasn't happened yet.
/// Pure so this resolution step is unit-testable without a live claim
/// object.
pub(crate) fn resource_claim_object_name(
    pod_claim_name: &str,
    resource_claim_name: Option<&str>,
    statuses: Option<&Vec<k8s_openapi::api::core::v1::PodResourceClaimStatus>>,
) -> Option<String> {
    if let Some(name) = resource_claim_name {
        return Some(name.to_string());
    }
    statuses?.iter().find(|s| s.name == pod_claim_name)?.resource_claim_name.clone()
}


/// A `ResourceClaim`'s `status.allocation.devices.results` grouped by
/// which driver's kubelet plugin needs to prepare them — a claim can
/// (rarely) span multiple drivers if its requests were satisfied by
/// different device classes, so each driver only ever gets asked about
/// its own devices. Pure given an already-fetched claim object, so the
/// grouping logic is unit-testable without a live ResourceClaim.
pub(crate) fn allocated_devices_by_driver(
    claim: &DraResourceClaim,
) -> BTreeMap<String, Vec<k8s_openapi::api::resource::v1beta1::DeviceRequestAllocationResult>> {
    let mut out: BTreeMap<String, Vec<_>> = BTreeMap::new();
    let results = claim
        .status
        .as_ref()
        .and_then(|s| s.allocation.as_ref())
        .and_then(|a| a.devices.as_ref())
        .and_then(|d| d.results.as_ref());
    for r in results.into_iter().flatten() {
        out.entry(r.driver.clone()).or_default().push(r.clone());
    }
    out
}


/// Whether `claim.status.reservedFor` lists this pod as a consumer (round
/// 64) — real kubelet gates `NodePrepareResources` on this before ever
/// touching a claim's devices, as a safety check against acting on a
/// claim not (yet, or no longer) reserved for this specific pod. **This
/// field is written by the scheduler at bind time, not by kubelet** —
/// round 63's docs incorrectly described this as something real kubelet
/// writes and nodelet was missing; corrected here. `resource: "pods"` and
/// an empty/absent `apiGroup` are how a Pod consumer reference is
/// spelled (pods are a core-API type). Pure given an already-fetched
/// claim, so unit-testable without a live object.
pub(crate) fn pod_is_reserved_for_claim(claim: &DraResourceClaim, pod_name: &str, pod_uid: &str) -> bool {
    claim
        .status
        .as_ref()
        .and_then(|s| s.reserved_for.as_ref())
        .into_iter()
        .flatten()
        .any(|r| r.resource == "pods" && r.name == pod_name && r.uid == pod_uid)
}

/// A container's `resources.claims[].{name, request}` entry -> the CDI
/// device IDs it should get, looked up from the pod-wide
/// `resolve_pod_claim_devices()` result. An unset `request` means "the
/// whole claim" (every device any request in it resolved to); a set one
/// filters to just that request's devices — matching either the request
/// name exactly, or `<request>/<subrequest>` (a `firstAvailable`
/// subrequest match, same prefix rule real kubelet's own matching uses).
/// A pod-claim name with no entry in `prepared` at all (fetch/prepare
/// failed, or a template claim not yet resolved) yields no devices rather
/// than an error — same graceful-degradation posture as a device-plugin
/// `Allocate()` failure.
pub(crate) fn cdi_devices_for_container_claim(claim_name: &str, request: Option<&str>, prepared: &HashMap<String, PreparedPodClaim>) -> Vec<String> {
    let Some(p) = prepared.get(claim_name) else { return Vec::new() };
    match request {
        None => p.all.clone(),
        Some(want) => p
            .by_request
            .iter()
            .filter(|(name, _)| name.as_str() == want || name.starts_with(&format!("{want}/")))
            .flat_map(|(_, ids)| ids.iter().cloned())
            .collect(),
    }
}


/// One pod-claim resolved to a real `ResourceClaim` object and confirmed
/// reserved for this pod — the intermediate result `resolve_pod_claim_devices()`
/// builds before it knows which drivers to call; kept as a named struct
/// (rather than a tuple) purely for readability at the call site.
struct ResolvedClaim {
    pod_claim_name: String,
    claim_name: String,
    claim_uid: String,
    by_driver: BTreeMap<String, Vec<k8s_openapi::api::resource::v1beta1::DeviceRequestAllocationResult>>,
}

impl CriRuntime {
    /// Dynamic Resource Allocation (round 63; batched + reservedFor-gated
    /// round 64): resolve every `spec.resourceClaims` entry this pod
    /// declares to the CDI device IDs its allocated devices' driver(s)
    /// hand back from `NodePrepareResources`, keyed by the pod-claim's own
    /// name (`spec.resourceClaims[].name`) — exactly the key
    /// `container.resources.claims[].name` references. Called once per
    /// `ensure_pod()` reconcile, before any container is created,
    /// mirroring how `overhead`/`cgroup_parent` are computed upfront. A
    /// pod with no `resourceClaims` costs nothing here — no API calls at
    /// all, same "zero cost when unused" shape as CPU/Memory Manager.
    ///
    /// Gated on `pod_is_reserved_for_claim()` (round 64): real kubelet
    /// won't touch a claim's devices until the scheduler has recorded this
    /// pod in `status.reservedFor`, a safety check against acting on a
    /// claim not (yet, or no longer) actually reserved for this pod. A
    /// claim not yet reserved isn't an error — it's a normal timing
    /// window before the scheduler finishes binding — so it's just
    /// skipped this reconcile; a later one (triggered by the Pod's own
    /// resync) retries.
    ///
    /// Batched per driver (round 64): every claim any of this pod's
    /// containers need from the *same* driver goes into one
    /// `NodePrepareResources` call, not one call per claim — matching how
    /// the real protocol's `claims` field is meant to be used.
    ///
    /// What nodelet does NOT do here, because it's not kubelet's job:
    /// *allocate* a claim (pick which devices satisfy it), or write
    /// `status.reservedFor` — both are the scheduler's/a DRA driver's own
    /// controller's job. Nodelet only ever reads state already written by
    /// those components.
    pub(crate) async fn resolve_pod_claim_devices(&self, pod: &Pod) -> HashMap<String, PreparedPodClaim> {
        let mut out = HashMap::new();
        let Some(pod_claims) = pod.spec.as_ref().and_then(|s| s.resource_claims.as_ref()) else { return out };
        if pod_claims.is_empty() {
            return out;
        }
        let namespace = pod.metadata.namespace.clone().unwrap_or_default();
        let pod_name = pod.metadata.name.clone().unwrap_or_default();
        let pod_uid = pod.metadata.uid.clone().unwrap_or_default();
        let statuses = pod.status.as_ref().and_then(|s| s.resource_claim_statuses.as_ref());
        let claims_api: Api<DraResourceClaim> = Api::namespaced(self.client.clone(), &namespace);

        let mut resolved = Vec::new();
        for pc in pod_claims {
            let Some(claim_name) = resource_claim_object_name(&pc.name, pc.resource_claim_name.as_deref(), statuses) else {
                // Template-based claim whose generated name hasn't shown up
                // in status yet (the resource-claim controller hasn't
                // created it, or the apiserver update hasn't propagated) —
                // nothing to prepare this reconcile; a later reconcile
                // (triggered by the Pod's own status update) will retry.
                continue;
            };
            let claim = match claims_api.get(&claim_name).await {
                Ok(c) => c,
                Err(e) => {
                    warn!(pod_claim = %pc.name, claim = %claim_name, error = ?e, "DRA: failed to fetch ResourceClaim; container(s) referencing it will start without these devices");
                    continue;
                }
            };
            if !pod_is_reserved_for_claim(&claim, &pod_name, &pod_uid) {
                debug!(pod_claim = %pc.name, claim = %claim_name, "DRA: claim not yet reserved for this pod (status.reservedFor) — skipping this reconcile, will retry");
                continue;
            }
            let claim_uid = claim.metadata.uid.clone().unwrap_or_default();
            let by_driver = allocated_devices_by_driver(&claim);
            resolved.push(ResolvedClaim { pod_claim_name: pc.name.clone(), claim_name, claim_uid, by_driver });
        }

        // Group every resolved claim's devices by driver, across ALL of
        // this pod's claims, so each driver gets exactly one
        // NodePrepareResources call covering everything it owns.
        let mut per_driver: HashMap<String, Vec<crate::dra::ClaimRef>> = HashMap::new();
        for rc in &resolved {
            for driver in rc.by_driver.keys() {
                if !self.dra.driver_configured(driver) {
                    warn!(driver = %driver, claim = %rc.claim_name, "DRA: no registered driver for this claim's allocated devices; container(s) referencing it will start without them");
                    continue;
                }
                let refs = per_driver.entry(driver.clone()).or_default();
                if !refs.iter().any(|c| c.uid == rc.claim_uid) {
                    refs.push(crate::dra::ClaimRef { namespace: namespace.clone(), uid: rc.claim_uid.clone(), name: rc.claim_name.clone() });
                }
            }
        }

        let mut results_by_claim_uid: HashMap<String, Result<Vec<crate::dra::PreparedDevice>, String>> = HashMap::new();
        for (driver, claim_refs) in per_driver {
            match self.dra.prepare(&driver, &claim_refs).await {
                Ok(per_claim) => results_by_claim_uid.extend(per_claim),
                Err(e) => warn!(driver = %driver, error = ?e, "DRA: NodePrepareResources failed for this driver's whole batch; container(s) referencing its claims will start without them"),
            }
        }

        for rc in resolved {
            let mut prepared = PreparedPodClaim::default();
            match results_by_claim_uid.get(&rc.claim_uid) {
                Some(Ok(devices)) => {
                    for d in devices {
                        for request in &d.request_names {
                            prepared.by_request.entry(request.clone()).or_default().extend(d.cdi_device_ids.iter().cloned());
                        }
                        prepared.all.extend(d.cdi_device_ids.iter().cloned());
                    }
                }
                Some(Err(e)) => warn!(claim = %rc.claim_name, error = %e, "DRA: NodePrepareResources reported a failure for this claim; container(s) referencing it will start without these devices"),
                None => {} // driver never configured / batch call failed — already warned above
            }
            out.insert(rc.pod_claim_name, prepared);
        }
        out
    }

    /// Teardown counterpart to `resolve_pod_claim_devices()` — called from
    /// `remove_pod()` (mirrors `unmount_csi_volumes()`'s placement/shape
    /// exactly: re-derive from the Pod object rather than tracking prepared
    /// state separately). No `reservedFor` gate here: a claim already
    /// prepared needs releasing regardless of its current reservation
    /// state (the scheduler/GC controller's own `reservedFor` cleanup is
    /// independent of, and not a prerequisite for, this node giving back
    /// whatever it locally prepared). Batched per driver (round 64), same
    /// reasoning as `resolve_pod_claim_devices()`. Best-effort: one
    /// driver being unreachable must not stop the rest of teardown.
    pub(crate) async fn unprepare_pod_claim_devices(&self, pod: &Pod) {
        let Some(pod_claims) = pod.spec.as_ref().and_then(|s| s.resource_claims.as_ref()) else { return };
        if pod_claims.is_empty() {
            return;
        }
        let namespace = pod.metadata.namespace.clone().unwrap_or_default();
        let statuses = pod.status.as_ref().and_then(|s| s.resource_claim_statuses.as_ref());
        let claims_api: Api<DraResourceClaim> = Api::namespaced(self.client.clone(), &namespace);

        let mut per_driver: HashMap<String, Vec<crate::dra::ClaimRef>> = HashMap::new();
        for pc in pod_claims {
            let Some(claim_name) = resource_claim_object_name(&pc.name, pc.resource_claim_name.as_deref(), statuses) else { continue };
            let claim = match claims_api.get(&claim_name).await {
                Ok(c) => c,
                Err(e) => {
                    warn!(pod_claim = %pc.name, claim = %claim_name, error = ?e, "DRA teardown: failed to fetch ResourceClaim; driver(s) not notified to unprepare");
                    continue;
                }
            };
            let claim_uid = claim.metadata.uid.clone().unwrap_or_default();
            for driver in allocated_devices_by_driver(&claim).into_keys() {
                if !self.dra.driver_configured(&driver) {
                    continue;
                }
                let refs = per_driver.entry(driver).or_default();
                if !refs.iter().any(|c| c.uid == claim_uid) {
                    refs.push(crate::dra::ClaimRef { namespace: namespace.clone(), uid: claim_uid.clone(), name: claim_name.clone() });
                }
            }
        }

        for (driver, claim_refs) in per_driver {
            match self.dra.unprepare(&driver, &claim_refs).await {
                Ok(results) => {
                    for (claim_uid, result) in results {
                        if let Err(e) = result {
                            warn!(driver = %driver, claim_uid = %claim_uid, error = %e, "DRA teardown: NodeUnprepareResources reported a failure for this claim");
                        }
                    }
                }
                Err(e) => warn!(driver = %driver, error = ?e, "DRA teardown: NodeUnprepareResources failed for this driver's whole batch"),
            }
        }
    }
}
