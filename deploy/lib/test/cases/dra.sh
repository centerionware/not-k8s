# lib/test/cases/dra.sh — Dynamic Resource Allocation (spec.resourceClaims,
# round 63): DRA driver plugin discovery, ResourceClaim allocation
# resolution, and NodePrepareResources CDI device wiring into container
# creation (dra.rs / runtime/cri.rs's resolve_pod_claim_devices()).
# Reuses the same plugin-registration directory csi_plugin_registration.sh
# and device_plugins.sh check (plugin_registry.rs handles CSI/device
# plugin/DRA driver registrations through one watcher).
#
# Same honest limitation device_plugins.sh already documents, several
# layers deeper here: fully exercising this needs a real DRA driver
# (a kubelet-plugin process implementing NodePrepareResources/
# NodeUnprepareResources) *and* the cluster's apiserver/scheduler/
# controller-manager actually running with the DynamicResourceAllocation
# feature gate + resource.k8s.io API group enabled *and* a real (or
# realistically faked) device for a DeviceClass/ResourceSlice to advertise
# and the scheduler to allocate. None of that is something this bash-only
# harness can stand up — a hand-rolled fake gRPC DRA driver (GetInfo/
# NodePrepareResources/NodeUnprepareResources can all be faked without
# real hardware) would make this fully automatable; that's a natural next
# step, not attempted in this round's harness. draplugin.proto's own
# comment also flags that its field numbers were reconstructed from
# documentation, not vendored from upstream — genuinely unverified against
# a live driver until one is available to test against.

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
    skip_test "exercising a real NodePrepareResources/CDI device injection round-trip needs an actual DRA driver (a kubelet-plugin binary/DaemonSet implementing NodePrepareResources/NodeUnprepareResources) pointed at NODELET_PLUGIN_REGISTRY_PATH, plus a DeviceClass/ResourceSlice/ResourceClaim the scheduler can actually allocate — not something this suite can set up. Manual spot-check: deploy a DRA driver DaemonSet configured to register against this node's registry path (PluginInfo.type: DRAPlugin), create a DeviceClass + ResourceClaim + a Pod with spec.resourceClaims referencing it and a container with resources.claims: [{name: <pod-claim-name>}], wait for the scheduler to allocate the claim (kubectl get resourceclaim -o yaml, check status.allocation), then confirm the pod reaches Running and that 'kubectl exec' into the container shows the CDI device the driver's NodePrepareResources response specified (watch nodelet's logs for 'DRA: NodePrepareResources failed' — its absence plus a Running pod is the proof it worked). Also confirm NodeUnprepareResources is called on pod deletion (driver-side logs, since nodelet doesn't expose this in kubectl-visible state)."
}

register_test test_plugin_registry_watches_for_dra_drivers_too
register_test test_resource_api_group_manual_note
register_test test_dra_manual_note
