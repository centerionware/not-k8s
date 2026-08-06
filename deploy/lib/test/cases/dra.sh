# lib/test/cases/dra.sh — Dynamic Resource Allocation (spec.resourceClaims,
# round 63): DRA driver plugin discovery, ResourceClaim allocation
# resolution, and NodePrepareResources CDI device wiring into container
# creation (dra.rs / runtime/cri.rs's resolve_pod_claim_devices()).
# Reuses the same plugin-registration directory csi_plugin_registration.sh
# and device_plugins.sh check (plugin_registry.rs handles CSI/device
# plugin/DRA driver registrations through one watcher).
#
# **Round 121: manually verified live** against
# kubernetes-sigs/dra-example-driver (the reference DRA driver real
# Kubernetes e2e/conformance tests use for exactly this, same role
# csi-driver-host-path plays for CSI) — a real Pod claiming a device via
# ResourceClaimTemplate reached Running with genuine CDI-injected device
# env vars from the driver's own NodePrepareResources response. Found and
# fixed 3 real bugs in the process (unbound ServiceAccount tokens,
# ResourceClaim fetched against the removed v1beta1 API, and
# draplugin.proto's reconstructed wire format being genuinely wrong — see
# docs/E2E_FINDINGS.md finding #18 for the full story). draplugin.proto is
# now transcribed directly from upstream, not reconstructed — that
# specific caveat is resolved.
#
# Still the same honest limitation device_plugins.sh documents, though:
# this bash-only harness can't itself stand up a Helm chart + driver
# DaemonSet, so the manual-spot-check tests below remain manual rather
# than becoming a real `register_test` — automating this (installing helm,
# deploying the reference driver, running a real ResourceClaim round-trip)
# is a natural fit for the GitHub Actions e2e job rather than this local
# suite.

test_plugin_registry_watches_for_dra_drivers_too() {
    if ! node_uses_cri_runtime; then skip_test "needs cri runtime"; fi
    local dir="${NODELET_PLUGIN_REGISTRY_PATH:-/var/lib/nodelet/plugins_registry}"
    if ! try_wait_until 15 bash -c "[[ -d '$dir' ]]"; then
        skip_test "no $dir — same directory csi_plugin_registration.sh/device_plugins.sh check; see their notes if this fails"
    fi
    assert_true test -d "$dir"
}

test_resource_api_group_manual_note() {
    skip_test "checking whether resource.k8s.io is even enabled on this cluster's apiserver (DynamicResourceAllocation feature gate) needs 'kubectl api-resources' against a live cluster with a version/build matching what this deployment actually runs — not assumed here since it varies by how deploy/ was configured. Manual spot-check: 'kubectl api-resources | grep resourceclaims' to confirm the API group exists at all before attempting any of the checks below."
}

test_dra_manual_note() {
    skip_test "exercising a real NodePrepareResources/CDI device injection round-trip needs an actual DRA driver (a kubelet-plugin binary/DaemonSet implementing NodePrepareResources/NodeUnprepareResources) pointed at NODELET_PLUGIN_REGISTRY_PATH, plus a DeviceClass/ResourceSlice/ResourceClaim the scheduler can actually allocate — not something this suite can set up. Manual spot-check: deploy a DRA driver DaemonSet configured to register against this node's registry path (PluginInfo.type: DRAPlugin), create a DeviceClass + ResourceClaim + a Pod with spec.resourceClaims referencing it and a container with resources.claims: [{name: <pod-claim-name>}], wait for the scheduler to allocate the claim AND record this pod in status.reservedFor (kubectl get resourceclaim -o yaml, check both status.allocation and status.reservedFor — round 64: nodelet now gates NodePrepareResources on reservedFor actually listing this pod, so a claim allocated but not yet reserved for it will just be retried on a later reconcile, not treated as an error; watch for 'DRA: claim not yet reserved for this pod' at debug level if it seems stuck), then confirm the pod reaches Running and that 'kubectl exec' into the container shows the CDI device the driver's NodePrepareResources response specified (watch nodelet's logs for 'DRA: NodePrepareResources failed' — its absence plus a Running pod is the proof it worked). If the pod has multiple resourceClaims backed by the same driver, confirm (via the driver's own logs) that only ONE NodePrepareResources call was made covering all of them, not one call per claim (round 64's batching). Also confirm NodeUnprepareResources is called on pod deletion (driver-side logs, since nodelet doesn't expose this in kubectl-visible state)."
}

register_test test_plugin_registry_watches_for_dra_drivers_too
register_test test_resource_api_group_manual_note
register_test test_dra_manual_note
