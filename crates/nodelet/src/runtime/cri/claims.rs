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


impl CriRuntime {
    /// Dynamic Resource Allocation (round 63): resolve every
    /// `spec.resourceClaims` entry this pod declares to the CDI device IDs
    /// its allocated devices' driver(s) hand back from `NodePrepareResources`,
    /// keyed by the pod-claim's own name (`spec.resourceClaims[].name`) —
    /// exactly the key `container.resources.claims[].name` references.
    /// Called once per `ensure_pod()` reconcile, before any container is
    /// created, mirroring how `overhead`/`cgroup_parent` are computed
    /// upfront. A pod with no `resourceClaims` costs nothing here — no API
    /// calls at all, same "zero cost when unused" shape as CPU/Memory
    /// Manager.
    ///
    /// What nodelet does NOT do here, because it's not kubelet's job:
    /// *allocate* a claim (pick which devices satisfy it) — that's the
    /// scheduler's/a DRA driver's own controller's job, already done by
    /// the time `status.allocation` is set. Nodelet only ever reads an
    /// allocation already made. Also not implemented: writing this node
    /// into `status.reservedFor` (real kubelet does, as a
    /// still-in-use marker for the claim-deallocation safety check) — see
    /// docs/GAP_CLOSURE.md's round 63 "known scope limitation."
    pub(crate) async fn resolve_pod_claim_devices(&self, pod: &Pod) -> HashMap<String, PreparedPodClaim> {
        let mut out = HashMap::new();
        let Some(pod_claims) = pod.spec.as_ref().and_then(|s| s.resource_claims.as_ref()) else { return out };
        if pod_claims.is_empty() {
            return out;
        }
        let namespace = pod.metadata.namespace.clone().unwrap_or_default();
        let statuses = pod.status.as_ref().and_then(|s| s.resource_claim_statuses.as_ref());
        let claims_api: Api<DraResourceClaim> = Api::namespaced(self.client.clone(), &namespace);

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
            let claim_uid = claim.metadata.uid.clone().unwrap_or_default();
            let by_driver = allocated_devices_by_driver(&claim);
            let mut prepared = PreparedPodClaim::default();
            for driver in by_driver.into_keys() {
                if !self.dra.driver_configured(&driver) {
                    warn!(driver = %driver, claim = %claim_name, "DRA: no registered driver for this claim's allocated devices; container(s) referencing it will start without them");
                    continue;
                }
                let claim_ref = crate::dra::ClaimRef { namespace: namespace.clone(), uid: claim_uid.clone(), name: claim_name.clone() };
                match self.dra.prepare(&driver, &claim_ref).await {
                    Ok(devices) => {
                        for d in devices {
                            for request in &d.request_names {
                                prepared.by_request.entry(request.clone()).or_default().extend(d.cdi_device_ids.iter().cloned());
                            }
                            prepared.all.extend(d.cdi_device_ids);
                        }
                    }
                    Err(e) => warn!(driver = %driver, claim = %claim_name, error = ?e, "DRA: NodePrepareResources failed; container(s) referencing it will start without these devices"),
                }
            }
            out.insert(pc.name.clone(), prepared);
        }
        out
    }

    /// Teardown counterpart to `resolve_pod_claim_devices()` — called from
    /// `remove_pod()` (mirrors `unmount_csi_volumes()`'s placement/shape
    /// exactly: re-derive from the Pod object rather than tracking prepared
    /// state separately). Best-effort per claim: one driver being
    /// unreachable must not stop the rest of teardown.
    pub(crate) async fn unprepare_pod_claim_devices(&self, pod: &Pod) {
        let Some(pod_claims) = pod.spec.as_ref().and_then(|s| s.resource_claims.as_ref()) else { return };
        if pod_claims.is_empty() {
            return;
        }
        let namespace = pod.metadata.namespace.clone().unwrap_or_default();
        let statuses = pod.status.as_ref().and_then(|s| s.resource_claim_statuses.as_ref());
        let claims_api: Api<DraResourceClaim> = Api::namespaced(self.client.clone(), &namespace);

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
                let claim_ref = crate::dra::ClaimRef { namespace: namespace.clone(), uid: claim_uid.clone(), name: claim_name.clone() };
                if let Err(e) = self.dra.unprepare(&driver, &claim_ref).await {
                    warn!(driver = %driver, claim = %claim_name, error = ?e, "DRA teardown: NodeUnprepareResources failed");
                }
            }
        }
    }

}
